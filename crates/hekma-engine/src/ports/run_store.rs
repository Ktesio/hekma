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
use crate::domain::{
    BreachAction, BudgetBreachEvent, EngineEvent, InstanceName, LatchKey, Micros, OutboxEntry,
    Rate, RecordOutcome, RunId, TransitionEvent, UsageEvent, UsageTotals,
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
