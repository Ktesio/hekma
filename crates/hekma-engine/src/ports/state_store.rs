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
use super::memory_backing::MemoryBackingKind;
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
    /// Story 14-2 (D4, spine AD-19): the last ACP session id this engine
    /// established for an `acp` instance, written by the start path at the
    /// point the handshake establishes it. `None` for every non-acp kind, for
    /// a record whose handshake has not completed yet (the fresh-spawn record
    /// commits with `None` and the id lands post-handshake), and for a pre-v8
    /// record. The id is the durable input for the NEXT start's `session/load`
    /// resume offer: the ACP transport itself is a pipe pair that dies with
    /// the engine that held it (an adopted process is not re-piped), so the
    /// resume rides the next handshake. `settle_spawn_record` retains the id
    /// across a stop/crash settle on a pid-0 seed row.
    pub acp_session_id: Option<String>,
    /// Story 15-2: the Run id (spine AD-7) this spawn opened, minted by the
    /// start/restart path and committed WITH the write-ahead record — the
    /// durable Run identity. `None` for a pre-v9 record (adoption mints and
    /// writes one back, surfacing a one-time note) and for the pid-0 policy /
    /// settle-retained seed rows (a stopped or never-started instance has no
    /// current Run; the next `start` mints fresh). ADOPTION resumes the
    /// persisted id instead of minting, so per-run budgets/usage and the
    /// breach latch survive across the N commands of a detached instance's
    /// life; a genuine restart (stop→start, or the reaper's restart path)
    /// rewrites the record with a freshly minted id and the Run resets.
    pub run_id: Option<RunId>,
    /// The LIVE-OWNER adoption boundary: the process fingerprint of the
    /// ENGINE PROCESS that spawned (or, after a cross-engine adoption,
    /// re-holds) this supervised process — the durable answer to "is the
    /// engine that owns this agent still ALIVE?".
    ///
    /// `Engine::open`'s orphan adoption reads it for every NON-detached
    /// record: an owner that is STILL ALIVE (and is not this process) means
    /// the agent is supervised by a live engine elsewhere, so the opening
    /// engine SKIPS the record entirely — it must not re-hold an armed
    /// (kill-on-drop) handle whose teardown at the opening command's exit
    /// would stop another process's agent (the `hekma agent list`-kills-
    /// `serve`'s-agent bug this field closes). A DEAD owner (the crashed
    /// engine of story 1-6) adopts exactly as before, and the adopting
    /// engine RE-STAMPS itself as the owner — so a `serve` that adopted a
    /// crash's orphans is itself protected from the next benign command.
    ///
    /// `None` for pre-boundary records (a v9 row read by a v10 engine — the
    /// migration leaves the columns NULL) and for a spawn whose own
    /// start-time token could not be read (the degraded-OS fallback): both
    /// keep the pre-boundary adoption posture (adopt armed). Detached
    /// records ignore the owner entirely — their adoption re-holds DISARMED
    /// either way (the durable-detach promise is unaffected).
    pub owner: Option<ProcessFingerprint>,
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

    /// Persist the established ACP session id onto the instance's spawn record
    /// (story 14-2, D4, spine AD-19) — the single session-id WRITE, called by
    /// the start path at the point the handshake establishes it
    /// (post-`session/new` / post-`session/load`). `None` clears (a record
    /// whose session is being re-established). Fails with
    /// [`StoreError::NotFound`] when no record row exists — the caller
    /// surfaces that (a dropped resume id must never vanish silently, AI-18).
    fn set_acp_session_id(
        &self,
        name: &InstanceName,
        session_id: Option<&str>,
    ) -> Result<(), StoreError>;

    /// Persist the current [`RunId`] onto the instance's spawn record (story
    /// 15-2) — the durable Run identity's single WRITE beside the start path's
    /// record commit. The ADOPTION path is the other caller: for a pre-v9
    /// record (whose `run_id` reads `None`) it mints one and writes it back so
    /// the back-compat mint + its one-time note are genuinely one-time (the
    /// NEXT command resumes the id instead of minting again).
    ///
    /// The UPDATE is a COMPARE-AND-SET on the `fingerprint` the caller read
    /// the record under (`WHERE ... AND pid = ? AND start_time = ?`, story
    /// 15-2 review round 1): if a CONCURRENT stop→start replaced the record
    /// between the read and this write, the fresh record's own Run id wins —
    /// a stale adoption mint must never stomp it (the losing write surfaces
    /// as [`StoreError::Backend`] naming the race, never a silent `Ok`).
    /// Fails with [`StoreError::NotFound`] when no record row exists (a
    /// dropped Run identity must never vanish silently, AI-18).
    ///
    /// DEFAULTED (compile-compatible for external implementors, 0.6→0.7): a
    /// store that has not implemented durable Run identity fails LOUD with
    /// [`StoreError::Backend`] naming the operation — the engine surfaces
    /// that as the write-failure diagnostic (every re-adoption mints a fresh
    /// Run, the documented pre-v9 posture), never a silent no-op.
    fn set_spawn_run_id(
        &self,
        _name: &InstanceName,
        _run_id: &RunId,
        _fingerprint: &ProcessFingerprint,
    ) -> Result<(), StoreError> {
        Err(StoreError::Backend(
            "this StateStore implementation does not override set_spawn_run_id \
             (story 15-2 durable run identity): the engine surfaces this write failure \
             and keeps the documented pre-v9 mint-per-adoption posture"
                .to_string(),
        ))
    }

    /// Settle the write-ahead spawn record (story 14-2): clear the live
    /// record — a clean stop, a crash reconcile, a terminal settle — while
    /// RETAINING an established acp session id for the next start's resume
    /// offer. When the cleared record carries an `acp_session_id`, a
    /// policy-only seed row (pid 0 — the exact shape
    /// [`StateStore::set_restart_policy`] seeds and adoption already skips)
    /// is re-persisted carrying {policy, count, acp_session_id}. When it
    /// carries none (every non-acp kind; an acp instance with no completed
    /// handshake) this is byte-identical to [`StateStore::clear_spawn_record`].
    /// Returns whether a session id was retained. Idempotent: settling an
    /// absent record is success (`false`).
    fn settle_spawn_record(&self, name: &InstanceName) -> Result<bool, StoreError>;

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
    ///
    /// **Store-level seam — bypasses the registry guard (story 15-6 doc
    /// warning):** this method writes the attachment row with NO terminal-state
    /// guard, NO managed-directory handling, and NO cross-process atomicity
    /// against a concurrent lifecycle transition. The sanctioned writer is
    /// [`Registry::attach_memory`](crate::domain::Registry::attach_memory)
    /// (which routes through
    /// [`StateStore::attach_memory_backing_txn`]); this raw seam exists for
    /// MIGRATIONS and TESTS only. A caller that drives it directly can land a
    /// backing row on a running instance — exactly the forbidden hot-swap the
    /// transactional seam refuses.
    fn upsert_memory_backing(&self, backing: &MemoryBacking) -> Result<(), StoreError>;

    /// Clear the Memory Backing attachment for an instance (a detach).
    /// IDEMPOTENT: clearing when nothing is attached (or the instance is gone)
    /// is success — the desired end state already holds. METADATA ONLY: this
    /// never touches any directory on disk (A-4 — operator data stays).
    ///
    /// **Store-level seam — bypasses the registry guard (story 15-6 doc
    /// warning):** like [`StateStore::upsert_memory_backing`], this clears the
    /// row with NO terminal-state guard and NO cross-process atomicity — a
    /// caller that drives it directly can clear a backing from under a
    /// mid-start instance. The sanctioned writer is
    /// [`Registry::detach_memory`](crate::domain::Registry::detach_memory)
    /// (via [`StateStore::detach_memory_backing_txn`]); migrations and tests
    /// only.
    fn clear_memory_backing(&self, name: &InstanceName) -> Result<(), StoreError>;

    /// Read the attached [`MemoryBacking`] for an instance, or `None` if nothing
    /// is attached.
    fn get_memory_backing(&self, name: &InstanceName) -> Result<Option<MemoryBacking>, StoreError>;

    // ---- Transactional memory/lifecycle seams (story 15-6, spine AD-17/AD-18) ----
    //
    // The registry's mutex serializes memory ops only WITHIN one Engine
    // process. The real CLI shape is one Engine per command sharing a state
    // dir, so `start` in one terminal races `memory attach` in another across
    // the ONLY state both share: this SQLite file. These three methods move
    // the guard + conflict-check + write (attach/detach) and the `→ starting`
    // transition + authoritative backing read (start) into ONE
    // `BEGIN IMMEDIATE` transaction each — the crash-atomic `migrate_step`
    // pattern — so the cross-process windows close at the durable store
    // without touching the two-mutex model (AD-17/AD-18 stand).

    /// Attach a [`MemoryBacking`] atomically (story 15-6): the terminal-state
    /// guard (the instance's PERSISTED state must be one of
    /// `terminal_states`), the kind-conflict/idempotence check, and the row
    /// write all run inside ONE `BEGIN IMMEDIATE` transaction — a concurrent
    /// process cannot land a lifecycle transition inside the guard→write gap.
    ///
    /// Returns [`MemoryBackingTxn::Attached`] when a fresh row committed, or
    /// [`MemoryBackingTxn::Idempotent`] when the SAME kind was already
    /// attached (A-6: no rewrite — the row's original `attached_at` stands).
    /// A DIFFERENT kind refuses with
    /// [`StoreError::MemoryBackingKindConflict`]; a non-terminal state
    /// refuses with [`StoreError::MemoryBackingHotSwap`]. Every refusal rolls
    /// the transaction back: the row is byte-unchanged.
    ///
    /// METADATA ONLY like [`StateStore::upsert_memory_backing`]: the managed
    /// directory on disk is the CALLER's concern (the registry creates it
    /// after this commits — inert + idempotent either way).
    fn attach_memory_backing_txn(
        &self,
        name: &InstanceName,
        kind: MemoryBackingKind,
        terminal_states: &[LifecycleState],
    ) -> Result<MemoryBackingTxn, StoreError>;

    /// Detach the named instance's Memory Backing atomically (story 15-6):
    /// the terminal-state guard and the row clear run inside ONE
    /// `BEGIN IMMEDIATE` transaction, refusing with
    /// [`StoreError::MemoryBackingHotSwap`] (row byte-unchanged) when the
    /// instance sits outside `terminal_states` at COMMIT time. Idempotent
    /// like [`StateStore::clear_memory_backing`]: detaching with nothing
    /// attached is success.
    fn detach_memory_backing_txn(
        &self,
        name: &InstanceName,
        terminal_states: &[LifecycleState],
    ) -> Result<(), StoreError>;

    /// Persist a lifecycle transition AND snapshot the memory backing in ONE
    /// transaction (story 15-6) — start's AUTHORITATIVE backing read. The
    /// transition is CONDITIONAL: it commits only when the persisted state is
    /// still `expected_prior` (another command's transition first refuses
    /// with [`StoreError::StateConflict`]), and it commits only when the
    /// backing row still equals `expected_backing` (the start path's
    /// pre-check read; a mid-start attach/detach by another process refuses
    /// with [`StoreError::MemoryBackingChangedUnderStart`]). Every refusal
    /// rolls the transaction back — the instance keeps its prior state, no
    /// `starting` row ever lands, nothing launches against a stale view.
    ///
    /// On success returns the snapshot READ INSIDE the same transaction (the
    /// authoritative view the caller's launch then reflects — equal to
    /// `expected_backing` by construction).
    fn set_state_snapshotting_memory_backing(
        &self,
        name: &InstanceName,
        expected_prior: LifecycleState,
        new_state: LifecycleState,
        expected_backing: Option<MemoryBacking>,
    ) -> Result<Option<MemoryBacking>, StoreError>;
}

/// What [`StateStore::attach_memory_backing_txn`] decided (story 15-6).
///
/// RETENTION (why the outcome is a returned, public type rather than the
/// bare `Ok(())` a plain write would return): the fresh-vs-idempotent
/// distinction is observable state a caller may need — HOSTS embedding the
/// engine and TESTS pinning A-6's original-timestamp idempotence read the
/// outcome off the seam instead of re-reading (and racing) the row's
/// `attached_at`. The engine's own registry path does not branch on it
/// (both outcomes are successes; the row is already the truth it would
/// consult), which is exactly why the fact is handed to the caller rather
/// than dropped — a return type is the only surface where it survives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryBackingTxn {
    /// A fresh attachment row was committed (kind + `attached_at`).
    Attached,
    /// The SAME kind was already attached: NO row rewrite (A-6 idempotence —
    /// the row's original `attached_at` stands); the caller still self-heals
    /// the managed directory.
    Idempotent,
}

#[cfg(test)]
mod tests {
    //! Story 15-2 review round 1: the DEFAULTED trait-method body is a covered
    //! seam — an external store that has not implemented durable Run identity
    //! must fail LOUD (a named Backend error the engine surfaces as the
    //! write-failure diagnostic), never a silent `Ok(())` that would let the
    //! mint appear persisted while nothing was written.

    use super::*;

    /// A store that implements NOTHING — every call hits a defaulted body or
    /// an unimplemented required method (only the defaulted seam is called).
    struct BareStore;

    impl StateStore for BareStore {
        fn create_instance(&self, _instance: &AgentInstance) -> Result<(), StoreError> {
            unimplemented!("not the seam under test")
        }
        fn get_instance(&self, _name: &InstanceName) -> Result<Option<AgentInstance>, StoreError> {
            unimplemented!("not the seam under test")
        }
        fn list_instances(&self) -> Result<Vec<AgentInstance>, StoreError> {
            unimplemented!("not the seam under test")
        }
        fn delete_instance(&self, _name: &InstanceName) -> Result<(), StoreError> {
            unimplemented!("not the seam under test")
        }
        fn set_state(
            &self,
            _name: &InstanceName,
            _state: LifecycleState,
        ) -> Result<(), StoreError> {
            unimplemented!("not the seam under test")
        }
        fn count_usage_events(&self, _name: &InstanceName) -> Result<u64, StoreError> {
            unimplemented!("not the seam under test")
        }
        fn record_usage_event(
            &self,
            _event: &UsageEvent,
            _rate: Option<Rate>,
        ) -> Result<RecordOutcome, StoreError> {
            unimplemented!("not the seam under test")
        }
        fn usage_totals(&self, _name: &InstanceName) -> Result<UsageTotals, StoreError> {
            unimplemented!("not the seam under test")
        }
        fn run_usage_totals(
            &self,
            _name: &InstanceName,
            _run_id: &RunId,
        ) -> Result<UsageTotals, StoreError> {
            unimplemented!("not the seam under test")
        }
        fn cost_totals(&self, _name: &InstanceName) -> Result<Micros, StoreError> {
            unimplemented!("not the seam under test")
        }
        fn run_cost_totals(
            &self,
            _name: &InstanceName,
            _run_id: &RunId,
        ) -> Result<Micros, StoreError> {
            unimplemented!("not the seam under test")
        }
        fn upsert_spawn_record(&self, _record: &SpawnRecord) -> Result<(), StoreError> {
            unimplemented!("not the seam under test")
        }
        fn clear_spawn_record(&self, _name: &InstanceName) -> Result<(), StoreError> {
            unimplemented!("not the seam under test")
        }
        fn set_acp_session_id(
            &self,
            _name: &InstanceName,
            _session_id: Option<&str>,
        ) -> Result<(), StoreError> {
            unimplemented!("not the seam under test")
        }
        fn settle_spawn_record(&self, _name: &InstanceName) -> Result<bool, StoreError> {
            unimplemented!("not the seam under test")
        }
        fn get_spawn_record(
            &self,
            _name: &InstanceName,
        ) -> Result<Option<SpawnRecord>, StoreError> {
            unimplemented!("not the seam under test")
        }
        fn list_spawn_records(&self) -> Result<Vec<SpawnRecord>, StoreError> {
            unimplemented!("not the seam under test")
        }
        fn set_restart_count(
            &self,
            _name: &InstanceName,
            _restart_count: u32,
            _last_known_cause: Option<&str>,
        ) -> Result<(), StoreError> {
            unimplemented!("not the seam under test")
        }
        fn set_restart_policy(
            &self,
            _name: &InstanceName,
            _policy: RestartPolicy,
        ) -> Result<(), StoreError> {
            unimplemented!("not the seam under test")
        }
        fn upsert_memory_backing(&self, _backing: &MemoryBacking) -> Result<(), StoreError> {
            unimplemented!("not the seam under test")
        }
        fn clear_memory_backing(&self, _name: &InstanceName) -> Result<(), StoreError> {
            unimplemented!("not the seam under test")
        }
        fn get_memory_backing(
            &self,
            _name: &InstanceName,
        ) -> Result<Option<MemoryBacking>, StoreError> {
            unimplemented!("not the seam under test")
        }
        fn attach_memory_backing_txn(
            &self,
            _name: &InstanceName,
            _kind: MemoryBackingKind,
            _terminal_states: &[LifecycleState],
        ) -> Result<MemoryBackingTxn, StoreError> {
            unimplemented!("not the seam under test")
        }
        fn detach_memory_backing_txn(
            &self,
            _name: &InstanceName,
            _terminal_states: &[LifecycleState],
        ) -> Result<(), StoreError> {
            unimplemented!("not the seam under test")
        }
        fn set_state_snapshotting_memory_backing(
            &self,
            _name: &InstanceName,
            _expected_prior: LifecycleState,
            _new_state: LifecycleState,
            _expected_backing: Option<MemoryBacking>,
        ) -> Result<Option<MemoryBacking>, StoreError> {
            unimplemented!("not the seam under test")
        }
    }

    #[test]
    fn the_defaulted_set_spawn_run_id_fails_loud_naming_the_seam() {
        // NEVER Ok: a silent default would let the engine believe the minted
        // Run id was persisted (the one-time note would fire) while every
        // later command silently minted again — the exact pre-v9 behavior
        // the loud default exists to surface (AI-18).
        let err = BareStore
            .set_spawn_run_id(
                &InstanceName::new("away").unwrap(),
                &RunId::mint(),
                &ProcessFingerprint::new(42, 7),
            )
            .unwrap_err();
        match err {
            StoreError::Backend(detail) => {
                assert!(
                    detail.contains("does not override set_spawn_run_id"),
                    "the default must NAME the seam: {detail}"
                );
                assert!(
                    detail.contains("pre-v9 mint-per-adoption posture"),
                    "the default must name the documented fallback posture: {detail}"
                );
            }
            other => panic!("the defaulted body must return Backend, got {other:?}"),
        }
    }
}
