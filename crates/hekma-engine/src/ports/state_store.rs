//! The [`StateStore`] port (hexagonal, spine AD-1).
//!
//! Trait methods speak in domain types, never SQL. The SQLite implementation
//! ([`crate::store::SqliteStore`]) sits behind this port; the registry service
//! depends on the trait, not the concrete store.
//!
//! ## Synchronous this story (spine AD-13 is 1.4)
//!
//! These methods are plain synchronous functions. AD-13's async-first engine
//! and `blocking()` facade land in story 1.4; registration/removal are
//! filesystem + SQLite operations with no supervision or concurrency need, so
//! sync is the correct altitude now. The trait takes no runtime handle and
//! holds no global state, keeping it facade-friendly for the 1.4 migration.

use crate::domain::{
    AgentInstance, InstanceName, LifecycleState, Micros, Rate, RecordOutcome, RestartPolicy, RunId,
    UsageEvent, UsageTotals,
};

use super::memory_backing::MemoryBacking;
use super::{ProcessFingerprint, StoreError};

/// A write-ahead spawn record (spine AD-5) — the durable supervision state for
/// one Agent Instance, persisted BEFORE the process is treated as supervised.
///
/// AD-5's rule: "before exec completes, persist {instance id, PID, process
/// start-time fingerprint}". This record carries that fingerprint plus the
/// per-instance Restart Policy (AD-15 "per-instance configurable"), the current
/// consecutive-failure restart count (survives an engine restart so the CLI can
/// read it), and the last-known transition cause (so a reconcile-to-`failed` can
/// name why). It lives in the one SQLite state store (AD-6), keyed by instance
/// name; it is CLEARED on a clean stop so a normally-stopped instance is never
/// later mistaken for an orphan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnRecord {
    /// The Agent Instance this record supervises.
    pub name: InstanceName,
    /// The supervised process's durable fingerprint (`{ pid, start-time }`).
    pub fingerprint: ProcessFingerprint,
    /// The per-instance Restart Policy (AD-15).
    pub restart_policy: RestartPolicy,
    /// Consecutive-failure restart count (resets on a clean run). Persisted so
    /// it survives an engine restart and the CLI can surface it (AC9).
    pub restart_count: u32,
    /// The last-known transition cause label (e.g. the crash detail), used when
    /// reconciling a non-adopted record to `failed`. `None` if not yet set.
    pub last_known_cause: Option<String>,
    /// Story 12-1 AMENDMENT (review loop 1): whether the spawn was DETACHED
    /// (`kt agent start --detach`). Detached-ness is a CROSS-LIFETIME property:
    /// it rides this record so every later `adopt()` re-holds the handle
    /// DISARMED (Unix: Drop skips the group kill; Windows: the handle never
    /// kills at drop anyway), and a benign intervening command's engine exit
    /// leaves the detached agent alive. Without the flag on the record, the
    /// first benign command after a detached start would kill the agent at its
    /// engine exit — the exact known-bad state the amendment closes. `false`
    /// keeps the story 1-6 adoption semantics byte-for-byte (the adopting
    /// engine owns what it re-holds and tears it down at ITS clean exit).
    pub detach: bool,
}

/// Persistence port for registry + lifecycle + the Usage Ledger.
///
/// The surface grows additively per story. Story 3-1 adds the append-only
/// Usage-Ledger WRITE ([`StateStore::record_usage_event`]) and the token-total
/// READS ([`StateStore::usage_totals`] / [`StateStore::run_usage_totals`]) beside
/// the existing [`StateStore::count_usage_events`] (spine AD-6/AD-7).
pub trait StateStore {
    /// Insert a new instance row.
    ///
    /// Fails with [`StoreError::DuplicateName`] if an instance with the same
    /// name already exists — enforced by the `UNIQUE` constraint on
    /// `agent_instances.name`, not a pre-check (which would race).
    fn create_instance(&self, instance: &AgentInstance) -> Result<(), StoreError>;

    /// Update an instance's Lifecycle State (and its `updated_at`) in place.
    ///
    /// Called by the supervisor on every persisted lifecycle transition (story
    /// 1.4). Domain-typed (a [`LifecycleState`], not a SQL string) — the SQLite
    /// implementation maps it to the `state` column. Fails with
    /// [`StoreError::NotFound`] if no such instance exists. The `state` column
    /// already exists (schema v1); no migration is needed.
    fn set_state(&self, name: &InstanceName, state: LifecycleState) -> Result<(), StoreError>;

    /// Fetch an instance by name, or `None` if absent.
    fn get_instance(&self, name: &InstanceName) -> Result<Option<AgentInstance>, StoreError>;

    /// List every instance in the Fleet, ordered by name for determinism.
    fn list_instances(&self) -> Result<Vec<AgentInstance>, StoreError>;

    /// Delete an instance row by name.
    ///
    /// Fails with [`StoreError::NotFound`] if no such row exists, so callers
    /// can distinguish "removed" from "was never there". `ON DELETE CASCADE`
    /// cleans up the instance's `usage_events` rows (story 3-1 populates them).
    fn delete_instance(&self, name: &InstanceName) -> Result<(), StoreError>;

    /// Count Usage Ledger events for an instance.
    ///
    /// A freshly registered instance returns `0` (Epic 1's "empty Usage Ledger"
    /// proof); story 3-1 populates the table so a metered instance returns a real
    /// count. The Usage Ledger is table rows scoped by instance, not a file (AD-6).
    fn count_usage_events(&self, name: &InstanceName) -> Result<u64, StoreError>;

    // ---- Usage Ledger writes + reads (story 3-1, spine AD-6/AD-7) ----

    /// Append ONE [`UsageEvent`] to the Usage Ledger in its OWN transaction (AD-6:
    /// one transaction per usage event, the ≤1s durability bound). A single-
    /// statement INSERT.
    ///
    /// Idempotent on the event's `(instance_id, run_id, sequence)` key — the
    /// no-double-count DB invariant (AC-A). A re-delivered batch whose key already
    /// exists hits the `UNIQUE` index and is classified [`RecordOutcome::DuplicateReplay`]
    /// (a NO-OP, NOT an error): nothing is inserted, the ledger total is unchanged.
    /// A brand-new event returns [`RecordOutcome::Inserted`]. Fails with
    /// [`StoreError::NotFound`] if the instance row is gone (the FK target).
    ///
    /// This is the SOLE Usage-Ledger writer the engine's commit choke point calls;
    /// no other code path may mutate `usage_events` (the AD-7 single-writer
    /// invariant, which story 3-2's enforcement relies on).
    ///
    /// `rate` (story 3-3) is the EFFECTIVE per-direction [`Rate`] resolved AT COMMIT,
    /// persisted onto the row so historical dollars keep the Rate in force when
    /// consumed (no retroactive repricing — AC-A). `None` = no Rate configured at
    /// commit → the row contributes $0 to the derived cost (honestly absent — AC-B).
    /// It is stamped alongside the frozen [`UsageEvent`] token fields WITHOUT
    /// touching the adapter-facing wire type (AD-6: an engine-side ledger concern).
    fn record_usage_event(
        &self,
        event: &UsageEvent,
        rate: Option<Rate>,
    ) -> Result<RecordOutcome, StoreError>;

    /// The CUMULATIVE token totals for an instance — the sum of `input_tokens` /
    /// `output_tokens` over ALL its `usage_events` rows (every Run), the AD-6
    /// "rollup aggregates" summed on read. An absent instance (or one with no
    /// events) totals [`UsageTotals::zero`] (mirrors [`StateStore::count_usage_events`]).
    fn usage_totals(&self, name: &InstanceName) -> Result<UsageTotals, StoreError>;

    /// The PER-RUN token totals for an instance — the sum scoped to a single
    /// `(instance_id, run_id)` (AC-B: per-run totals reflect the `starting`→terminal
    /// span). An absent instance / unknown Run totals [`UsageTotals::zero`].
    fn run_usage_totals(
        &self,
        name: &InstanceName,
        run_id: &RunId,
    ) -> Result<UsageTotals, StoreError>;

    /// The CUMULATIVE derived DOLLAR cost for an instance (story 3-3, AC-A) — the
    /// sum over ALL its `usage_events` rows of each row's cost priced at THAT ROW'S
    /// OWN persisted Rate (no retroactive repricing). A row with a NULL rate (no
    /// Rate at commit, or a pre-v4 row) contributes [`Micros::ZERO`]. An absent
    /// instance totals [`Micros::ZERO`]. Overflow SATURATES (billing discipline).
    fn cost_totals(&self, name: &InstanceName) -> Result<Micros, StoreError>;

    /// The PER-RUN derived DOLLAR cost for an instance (story 3-3) — the same
    /// per-row-priced sum scoped to a single `(instance_id, run_id)`. An absent
    /// instance / unknown Run totals [`Micros::ZERO`].
    fn run_cost_totals(&self, name: &InstanceName, run_id: &RunId) -> Result<Micros, StoreError>;

    // ---- Write-ahead spawn records (story 1-6, spine AD-5/AD-6) ----

    /// Insert or replace the write-ahead spawn record for an instance, in ONE
    /// transaction (AD-6). Called by the supervisor BEFORE the process is
    /// treated as supervised ("no spawn without its record committed first").
    /// Fails with [`StoreError::NotFound`] if the instance row is gone.
    fn upsert_spawn_record(&self, record: &SpawnRecord) -> Result<(), StoreError>;

    /// Clear the write-ahead spawn record for an instance (a clean stop, so it is
    /// not later adopted/failed as an orphan). Idempotent: clearing an absent
    /// record is success.
    fn clear_spawn_record(&self, name: &InstanceName) -> Result<(), StoreError>;

    /// Read the write-ahead spawn record for an instance, or `None` if absent.
    fn get_spawn_record(&self, name: &InstanceName) -> Result<Option<SpawnRecord>, StoreError>;

    /// List every write-ahead spawn record (the reconcile input on engine start).
    /// Ordered by instance name for determinism.
    fn list_spawn_records(&self) -> Result<Vec<SpawnRecord>, StoreError>;

    /// Persist a new restart count + last-known cause for an instance's spawn
    /// record (a restart bump or a reset), in ONE transaction (AD-6). No-op if
    /// the instance has no spawn record. Kept distinct from
    /// [`StateStore::upsert_spawn_record`] so a restart bump does not need the
    /// full fingerprint again.
    fn set_restart_count(
        &self,
        name: &InstanceName,
        restart_count: u32,
        last_known_cause: Option<&str>,
    ) -> Result<(), StoreError>;

    /// Set the per-instance Restart Policy (AD-15 "per-instance configurable"),
    /// in ONE transaction (AD-6). If a spawn record exists its policy is updated;
    /// if none exists, a MINIMAL record is created carrying only the policy (a
    /// zero fingerprint + count 0) so the setting persists before the instance is
    /// ever started. Fails with [`StoreError::NotFound`] if the instance row is
    /// gone. This is the per-instance config SEED (Epic-2 layered TOML is later).
    fn set_restart_policy(
        &self,
        name: &InstanceName,
        policy: RestartPolicy,
    ) -> Result<(), StoreError>;

    // ---- Memory Backing attachments (story 5-1, spine AD-11/AD-6) ----

    /// Persist the attached [`MemoryBacking`] for an instance, in ONE transaction
    /// (AD-6). Exactly ONE backing per instance — the row is UNIQUE on the
    /// instance FK, so a re-attach REPLACES the row (kind + timestamp). Typed
    /// columns, never a JSON blob (DC-2). Fails with [`StoreError::NotFound`] if
    /// the instance row is gone.
    fn upsert_memory_backing(&self, backing: &MemoryBacking) -> Result<(), StoreError>;

    /// Clear the Memory Backing attachment for an instance (a detach).
    /// IDEMPOTENT: clearing when nothing is attached (or the instance is gone)
    /// is success — the desired end state already holds. METADATA ONLY: this
    /// never touches any directory on disk (A-4 — operator data stays).
    fn clear_memory_backing(&self, name: &InstanceName) -> Result<(), StoreError>;

    /// Read the attached [`MemoryBacking`] for an instance, or `None` if nothing
    /// is attached.
    fn get_memory_backing(&self, name: &InstanceName) -> Result<Option<MemoryBacking>, StoreError>;
}
