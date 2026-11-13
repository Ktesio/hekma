//! The Run-scale storage port (story 21-1, spine AD-27).
//!
//! [`StateStore`](super::StateStore) is frozen (a published port since the
//! v0.7.0 freeze), so the reads and writes the store writer adds go behind
//! this second port. It is shaped for a later external database (AD-27's
//! deferred Postgres backend) in two ways:
//!
//! * **Writes are batch-scoped.** Every write method runs inside the
//!   transaction the store writer opened for its current batch (one
//!   `BEGIN IMMEDIATE`, one SAVEPOINT per message). An implementation never
//!   commits on its own; the writer drives begin, savepoints and commit.
//! * **Reads are keyset-paginated.** A list read takes an exclusive `after`
//!   key and a bounded page size and returns a [`Page`] carrying the next key,
//!   so no read is O(history) under a lock.
//!
//! The trait is synchronous: it lives behind the writer thread (and the
//! Registry's read-only connection). An async backend would wrap itself inside
//! the writer rather than change this shape. It is crate-private until the
//! Run and Policy tables of stories 21.2 and 21.3 settle its surface.

use super::StoreError;
use crate::domain::credential::{TokenClass, TokenHash, TokenState};
use crate::domain::runs::policy::{PolicyRecord, PolicyResolution, ResolveInputs, RunPolicy};
use crate::domain::{
    BreachAction, BudgetBreachEvent, EngineEvent, InstanceName, Label, LabelSource, LatchKey,
    Micros, OutboxEntry, Rate, RecordOutcome, RunId, RunLimits, RunRecord, Selector,
    TransitionEvent, UsageEvent, UsageTotals,
};

/// The largest page any keyset read returns, whatever the caller asks for.
pub(crate) const PAGE_MAX: usize = 1_000;

/// Clamp a requested page size into `1..=PAGE_MAX` (`0` takes the maximum).
pub(crate) fn clamp_page(limit: usize) -> usize {
    if limit == 0 {
        PAGE_MAX
    } else {
        limit.min(PAGE_MAX)
    }
}

/// One keyset page: the items after the request key, in key order, plus the
/// key to pass as `after` for the next page (`None` once the read is caught
/// up — the page was not full).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<i64>,
}

impl<T> Page<T> {
    /// Build a page from `(key, item)` rows read with `LIMIT limit`: a full
    /// page carries its last key as `next`, a short one ends the read.
    pub(crate) fn from_rows(rows: Vec<(i64, T)>, limit: usize) -> Self {
        let next = if rows.len() == limit {
            rows.last().map(|(key, _)| *key)
        } else {
            None
        };
        Self {
            items: rows.into_iter().map(|(_, item)| item).collect(),
            next,
        }
    }
}

/// The outbox's retention facts (the old journal view's derived fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutboxBounds {
    /// The first retained seq, or 1 when the outbox holds nothing.
    pub retained_from: u64,
    /// The last retained seq, or 0 when the outbox holds nothing.
    pub tail: u64,
    /// The durable ack cursor (`None` = never acknowledged).
    pub ack: Option<u64>,
}

/// What one outbox append did: the seq it minted and how many entries the
/// retention cap evicted to make room.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OutboxAppend {
    pub seq: u64,
    pub dropped_acked: u64,
    pub dropped_unacked: u64,
}

/// A rollup's usage totals plus the derived cost (each row priced at its own
/// stored Rate when it was committed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rollup {
    pub totals: UsageTotals,
    pub cost: Micros,
}

impl Rollup {
    pub(crate) const fn zero() -> Self {
        Self {
            totals: UsageTotals::zero(),
            cost: Micros::ZERO,
        }
    }
}

/// The Run-scale storage port (AD-27). See the module docs.
pub(crate) trait RunStore {
    /// Insert one usage row stamped with its launch generation and the Rate
    /// in force, and fold it into the per-instance and per-Run rollups. A
    /// dedup-key hit is [`RecordOutcome::DuplicateReplay`] and touches
    /// nothing.
    fn insert_usage(
        &self,
        event: &UsageEvent,
        rate: Option<Rate>,
        generation: u64,
    ) -> Result<RecordOutcome, StoreError>;

    /// The cumulative rollup for an instance (zero when it has no rows).
    fn instance_rollup(&self, name: &InstanceName) -> Result<Rollup, StoreError>;

    /// The rollup for one Run of an instance (zero when it has no rows).
    fn run_rollup(&self, name: &InstanceName, run_id: &RunId) -> Result<Rollup, StoreError>;

    /// The rollup for one Run by id alone (story 23-1, schema v18: an
    /// instance-less wire Run has a rollup too).
    fn run_rollup_by_id(&self, run_id: &RunId) -> Result<Rollup, StoreError>;

    /// A Run's `(namespace, external id)` identity, when it has one (story
    /// 23-1: the wire Runs).
    fn run_identity(&self, run_id: &RunId) -> Result<Option<(String, String)>, StoreError>;

    /// Bump the instance's launch generation and return the new value.
    fn bump_launch_generation(&self, name: &InstanceName) -> Result<u64, StoreError>;

    /// Record one transition event for its instance.
    fn append_transition(&self, event: &TransitionEvent) -> Result<(), StoreError>;

    /// Record one breach event for its instance.
    fn append_breach(&self, event: &BudgetBreachEvent) -> Result<(), StoreError>;

    /// One keyset page of an instance's transition events (key = row id).
    fn transitions_page(
        &self,
        name: &InstanceName,
        after: i64,
        limit: usize,
    ) -> Result<Page<TransitionEvent>, StoreError>;

    /// One keyset page of an instance's breach events (key = row id).
    fn breaches_page(
        &self,
        name: &InstanceName,
        after: i64,
        limit: usize,
    ) -> Result<Page<BudgetBreachEvent>, StoreError>;

    /// The breach events recorded for one Run of an instance, in order.
    fn run_breaches(
        &self,
        name: &InstanceName,
        run_id: &RunId,
    ) -> Result<Vec<BudgetBreachEvent>, StoreError>;

    /// Latch `(dimension, scope)` for a Run under `action` (idempotent).
    fn latch(
        &self,
        run_id: &RunId,
        key: LatchKey,
        action: BreachAction,
        at: &str,
    ) -> Result<(), StoreError>;

    /// Unlatch one `(dimension, scope)` under `action` (the raise re-arm,
    /// story 21-3): the pair may fire again under the same action.
    fn unlatch(
        &self,
        run_id: &RunId,
        key: LatchKey,
        action: BreachAction,
    ) -> Result<(), StoreError>;

    /// Every latch recorded for a Run.
    fn latches(&self, run_id: &RunId) -> Result<Vec<(LatchKey, BreachAction)>, StoreError>;

    /// Append one committed event to the outbox, evicting under `cap` first
    /// (acked prefix, then the oldest un-acked, never the last entry).
    fn append_outbox(&self, event: &EngineEvent, cap: u64) -> Result<OutboxAppend, StoreError>;

    /// One keyset page of outbox entries with seq strictly after `after`.
    fn outbox_page(&self, after: u64, limit: usize) -> Result<Page<OutboxEntry>, StoreError>;

    /// The outbox's retention facts and ack cursor.
    fn outbox_bounds(&self) -> Result<OutboxBounds, StoreError>;

    /// Persist the ack cursor (the caller has already validated it).
    fn set_outbox_ack(&self, seq: u64) -> Result<(), StoreError>;
}

/// A Run to insert (story 21-2): its row, its labels, the limits set on it
/// and an optional `(binding, external id)` identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NewRun {
    pub record: RunRecord,
    pub labels: Vec<Label>,
    pub limits: RunLimits,
    pub identity: Option<(String, String)>,
}

/// The Policies and their per-Run resolutions (story 21-3, schema v15).
/// Reads run on either connection; writes run inside the store writer's
/// batch, like [`RunRecords`]'.
pub(crate) trait PolicyRecords {
    /// Every Policy, `default` included, ordered by name.
    fn policies(&self) -> Result<Vec<PolicyRecord>, StoreError>;

    /// One Policy by name.
    fn policy(&self, name: &str) -> Result<Option<PolicyRecord>, StoreError>;

    /// The RAW stored Selector text of one Policy, undecoded (`None` when
    /// no row has the name) — the corrupt-row delete path's read: a row
    /// whose Selector no longer parses fails every DECODED read, so the
    /// remediation (delete by name) must not depend on decoding.
    fn policy_selector_raw(&self, name: &str) -> Result<Option<Option<String>>, StoreError>;

    /// Insert or update a Policy and its Rate table.
    fn put_policy(&self, record: &PolicyRecord) -> Result<(), StoreError>;

    /// Delete a Policy (`default` is refused above this layer).
    fn delete_policy(&self, name: &str) -> Result<(), StoreError>;

    /// A Run's stored resolution, with the inputs that produced it.
    fn run_policy(&self, run_id: &RunId) -> Result<Option<RunPolicy>, StoreError>;

    /// Write a Run's stored resolution (replace wholesale).
    fn set_run_policy(
        &self,
        run_id: &RunId,
        resolution: &PolicyResolution,
        inputs: &ResolveInputs,
        at: &str,
    ) -> Result<(), StoreError>;

    /// Whether the instance is kept on `pause` by the FR-21 upgrade (a
    /// pending or finished marker row): the action resolves `pause` at the
    /// explicit level until a Breach Action is set explicitly.
    fn breach_default_kept(&self, instance: &str) -> Result<bool, StoreError>;

    /// The instances whose upgrade has not finished (the resume list).
    fn breach_default_pending(&self) -> Result<Vec<String>, StoreError>;

    /// Mark the named instances' upgrade finished, claiming only rows
    /// still pending, and return the names THIS call finished (stage 3's
    /// cross-open idempotence: the event fires for exactly these).
    fn finish_breach_default(
        &self,
        written: &[(String, bool)],
        at: &str,
    ) -> Result<Vec<String>, StoreError>;

    /// Clear an instance's marker (an explicit `budget.breach_action` write).
    fn clear_breach_default(&self, instance: &str) -> Result<(), StoreError>;

    /// One Run's resolution facts in one read: its row, labels, Run limits
    /// and whether its instance is kept on `pause` by the upgrade (the
    /// pipeline's live per-event resolution).
    fn run_resolution_facts(&self, run_id: &RunId) -> Result<Option<ResolutionFacts>, StoreError>;

    /// Every open process-owning Run's resolution facts plus every
    /// instance kept on `pause` by the upgrade, in a fixed handful of
    /// queries (the Fleet read's batch: one pass for the whole Fleet, not
    /// one per row). Each entry equals what [`Self::run_resolution_facts`]
    /// reads for that Run.
    fn open_owner_resolution_facts(&self) -> Result<OpenOwnerFacts, StoreError>;
}

/// The Fleet read's batched resolution facts (story 21-3's perf pass):
/// what [`PolicyRecords::open_owner_resolution_facts`] reads.
pub(crate) struct OpenOwnerFacts {
    /// Every open process-owning Run's facts (at most one per instance,
    /// the partial unique index), each as `run_resolution_facts` reads it.
    pub runs: Vec<ResolutionFacts>,
    /// The names of every instance kept on `pause` by the upgrade.
    pub kept: std::collections::HashSet<String>,
}

/// One Run's resolution facts (story 21-3): everything the live per-event
/// resolution reads in one store pass. The `labels` and `kept_on_pause`
/// arms are read by the stored-resolution path (`resolve_run`), which
/// re-reads them through its own store handle today.
#[allow(dead_code)]
pub(crate) struct ResolutionFacts {
    /// The Run's row.
    pub record: RunRecord,
    /// Its labels, sorted by key.
    pub labels: Vec<Label>,
    /// The limits set directly on it (the Operator/Host layer).
    pub limits: RunLimits,
    /// The limits a Caller set on it (their own layer: they apply only
    /// where tighter than the resolved value).
    pub caller_limits: RunLimits,
    /// Its instance is kept on `pause` by the FR-21 upgrade.
    pub kept_on_pause: bool,
}

/// Which layer of a Run's own limits a write replaces (story 21-6): the
/// Operator/Host layer outranks configuration and Policies; the Caller
/// layer applies only where it is tighter than everything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LimitSlot {
    /// Set by an Operator or a Host.
    Run,
    /// Set by a Caller.
    Caller,
}

/// The Run registry's storage (story 21-2, AD-25): the Run rows, their labels,
/// limits and identities. Writes run inside the store writer's batch, like
/// [`RunStore`]'s. Policies and their resolutions (21-3) ride along.
pub(crate) trait RunRecords: PolicyRecords {
    /// Insert a new Run with its labels, limits and identity.
    fn insert_run(&self, run: &NewRun) -> Result<(), StoreError>;

    /// One Run's row.
    fn run_record(&self, run_id: &RunId) -> Result<Option<RunRecord>, StoreError>;

    /// A Run's labels, sorted by key.
    fn run_labels(&self, run_id: &RunId) -> Result<Vec<Label>, StoreError>;

    /// The limits set directly on a Run (the Operator/Host layer).
    fn run_limits(&self, run_id: &RunId) -> Result<RunLimits, StoreError>;

    /// The limits a Caller set on a Run (story 21-6: their own layer, kept
    /// apart so they apply only while tighter than the resolved value).
    fn run_caller_limits(&self, run_id: &RunId) -> Result<RunLimits, StoreError>;

    /// Replace one layer of a Run's `run_limits` (story 21-6, the budget
    /// job): `limits` becomes that layer's whole set, plus the action row
    /// that decides the layer's Breach Action (set or explicitly unset). A
    /// row whose value does not change keeps its `source` and `set_at`;
    /// a new or changed row is written with `source` (the actor) and `at`.
    /// An empty `limits` deletes the layer's rows. The other layer is never
    /// touched.
    fn replace_run_limits(
        &self,
        run_id: &RunId,
        slot: LimitSlot,
        limits: &RunLimits,
        source: &str,
        at: &str,
    ) -> Result<(), StoreError>;

    /// The Run and its ancestors, the Run first.
    fn run_chain(&self, run_id: &RunId) -> Result<Vec<RunRecord>, StoreError>;

    /// The instance's open process-owning Run.
    fn open_process_run(&self, instance: &InstanceName) -> Result<Option<RunRecord>, StoreError>;

    /// The instance's NEWEST process-owning Run, ended or not (story 21-5):
    /// the subject of a transition published after its owner Run's end
    /// committed (a `stopped` or `failed` transition), so a Selector-
    /// filtered event stream still sees it. ULID ids sort by creation, and
    /// at most one owner is open, so the open owner is the newest when it
    /// exists.
    fn latest_process_run(&self, instance: &InstanceName) -> Result<Option<RunRecord>, StoreError>;

    /// The non-ended descendants of a Run hosted in the same process (same
    /// instance, not process owners), oldest first.
    fn open_in_process_descendants(&self, run_id: &RunId) -> Result<Vec<RunRecord>, StoreError>;

    /// Persist a Run's mutable fields (depth, wire path, status, Anomalies,
    /// end fields, update time).
    fn save_run(&self, record: &RunRecord) -> Result<(), StoreError>;

    /// Replace every label of one source on a Run. A Caller write never
    /// replaces a bound row (story 21-9): a Caller label on a key already
    /// bound is skipped at the SQL level, behind the domain's
    /// `check_caller_labels` refusal. The Caller-label write the announce and
    /// identity-header paths will use; tests exercise it until they land.
    #[cfg_attr(not(test), allow(dead_code))]
    fn replace_labels(
        &self,
        run_id: &RunId,
        source: LabelSource,
        labels: &[Label],
    ) -> Result<(), StoreError>;

    /// Replace one Run's whole label set (story 21-9): the label verb's and
    /// the configuration re-sync's one write.
    fn set_run_labels(&self, run_id: &RunId, labels: &[Label]) -> Result<(), StoreError>;

    /// The non-ended Runs carrying every `key=value` term of `selector`
    /// (bound or Caller), ordered by run id, at most `limit` (story 21-9).
    fn select_runs(&self, selector: &Selector, limit: usize) -> Result<Vec<RunRecord>, StoreError>;

    /// One keyset page of the Run listing (story 21-5): the view's rows
    /// after `after` (the decoded cursor's run id, `None` = the first
    /// page), newest run id first, plus the run id the next page resumes
    /// after (`None` when the read is exhausted). Reads the v16 partial
    /// indexes — no `SCAN` on `runs` (pinned by tests).
    fn list_runs(
        &self,
        view: crate::domain::runs::ListingView,
        selector: Option<&Selector>,
        after: Option<&str>,
        limit: usize,
        cutoff: &str,
    ) -> Result<(Vec<RunRecord>, Option<String>), StoreError>;

    /// The listing's counts (story 21-5): how many Runs the view lists and
    /// how many are healthy and open, both subject to `selector`.
    fn listing_counts(
        &self,
        view: crate::domain::runs::ListingView,
        selector: Option<&Selector>,
        cutoff: &str,
    ) -> Result<crate::domain::runs::ListingCounts, StoreError>;

    /// The Run an external id maps to under a binding.
    fn identity_run(&self, binding: &str, external_id: &str) -> Result<Option<RunId>, StoreError>;

    /// How many of a Run's children have not ended.
    fn open_children(&self, run_id: &RunId) -> Result<u64, StoreError>;

    /// Every non-ended Run (the snapshot's initial load).
    fn open_runs(&self) -> Result<Vec<RunRecord>, StoreError>;

    /// Make sure a run id the pipeline is about to latch has a Run row: an id
    /// with none (usage an older engine recorded under its own id after the
    /// upgrade) gets an `ended` placeholder, end cause `unregistered`, so the
    /// latch's reference holds and the usage commit never fails on it.
    fn ensure_run_row(&self, run_id: &RunId, instance: &InstanceName) -> Result<(), StoreError>;

    // Story 22-1: the credential port (`tokens`, schema v17). A hash leaves
    // the store layer only as a [`TokenHash`] (for the snapshot); every read
    // a surface renders is metadata.

    /// Insert one token row.
    fn insert_token(&self, token: &TokenRow) -> Result<(), StoreError>;

    /// The row for one hash — the in-transaction validity read every
    /// Caller-authorized job makes.
    fn token(&self, hash: &TokenHash) -> Result<Option<TokenRow>, StoreError>;

    /// Move one token to `state` (`retired` or `revoked`) at `at`, with its
    /// cause. Only an `active` row moves; returns whether it did.
    fn set_token_state(
        &self,
        hash: &TokenHash,
        state: TokenState,
        at: &str,
        cause: &str,
    ) -> Result<bool, StoreError>;

    /// Move one `retired` token (a used refresh credential) to `revoked`
    /// at `at`, with its cause: once revoked, presenting it again is a plain
    /// refusal, never reuse. Returns whether it moved.
    fn revoke_retired(&self, hash: &TokenHash, at: &str, cause: &str) -> Result<bool, StoreError>;

    /// A Run's active tokens, oldest first.
    fn active_tokens(&self, run_id: &RunId) -> Result<Vec<TokenRow>, StoreError>;

    /// Every token of a Run, any state, oldest first (the metadata listing
    /// and the cascade's roots).
    fn run_tokens(&self, run_id: &RunId) -> Result<Vec<TokenRow>, StoreError>;

    /// Every ACTIVE token whose `issued_by` chain starts at one of `roots`
    /// (the cascade walk: recursive over `issued_by_hash`), oldest first.
    fn active_issued_under(&self, roots: &[TokenHash]) -> Result<Vec<TokenRow>, StoreError>;

    /// Every RETIRED token (a used refresh credential) on the same walk as
    /// [`Self::active_issued_under`], oldest first.
    fn retired_issued_under(&self, roots: &[TokenHash]) -> Result<Vec<TokenRow>, StoreError>;

    /// Every active Run Key and announce credential of a non-ended Run (the
    /// snapshot's key index at open).
    fn active_key_rows(&self) -> Result<Vec<TokenRow>, StoreError>;

    /// Record a process-owning Run's upstream base URL (story 23-1).
    fn set_run_upstream(&self, run_id: &RunId, upstream: Option<&str>) -> Result<(), StoreError>;

    /// The Run Keys revoked at or after `since` (RFC 3339), newest first,
    /// at most `limit` (story 23-1: the recently-revoked set at open).
    fn recently_revoked_keys(
        &self,
        since: &str,
        limit: usize,
    ) -> Result<Vec<TokenHash>, StoreError>;

    /// What a Run's snapshot entry carries beyond its row (story 23-1):
    /// the disposition from its latch rows, its recorded upstream and its
    /// identity.
    fn entry_extras(
        &self,
        run_id: &RunId,
    ) -> Result<crate::domain::runs::snapshot::EntryExtras, StoreError>;
}

/// One stored credential (story 22-1, `tokens`): its hash and metadata,
/// never its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TokenRow {
    pub hash: TokenHash,
    pub token_id: String,
    pub class: TokenClass,
    pub run_id: RunId,
    pub launch_generation: Option<u64>,
    pub paired_hash: Option<TokenHash>,
    pub issued_by_hash: Option<TokenHash>,
    pub custody_eligible: bool,
    pub state: TokenState,
    pub issued_at: String,
    pub revoked_at: Option<String>,
    pub revoke_cause: Option<String>,
}
