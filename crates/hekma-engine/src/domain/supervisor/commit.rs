//! A lifecycle transition as ONE store-writer job (story 21-1, D1).
//!
//! The state write (plain, guarded on the prior state, or snapshotting the
//! memory backing), the `transition_events` row and the outbox row commit in
//! one transaction, so a transition is durable and exportable the moment it
//! is acknowledged. `instance.log` is a best-effort mirror written after
//! commit (see `Supervisor::after_transition_commit`), so the story-16-3
//! compensation for a lost log append is no longer needed.

use crate::domain::bus::EngineEvent;
use crate::domain::error::RegistryError;
use crate::domain::event::TransitionEvent;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::name::InstanceName;
use crate::domain::outbox::OUTBOX_MAX_BYTES;
use crate::domain::registry::Registry;
use crate::ports::{MemoryBacking, OutboxAppend, RunStore, StateStore};

/// How a transition writes the instance's state row.
pub(super) enum StateWrite {
    /// Unconditional.
    Plain,
    /// Only when the persisted state still equals the given prior (18-2 P3).
    IfPrior(LifecycleState),
    /// The `→ starting` shape (15-6): conditional on the prior state and the
    /// memory backing the start path read; returns the backing read inside
    /// the transaction.
    SnapshotBacking {
        expected_prior: LifecycleState,
        expected_backing: Option<MemoryBacking>,
    },
}

/// Commit `event`'s state write, event row and outbox row in one writer job.
pub(super) fn commit_transition(
    registry: &Registry,
    name: &InstanceName,
    write: StateWrite,
    event: &TransitionEvent,
) -> Result<(Option<MemoryBacking>, OutboxAppend), RegistryError> {
    let name = name.clone();
    let event = event.clone();
    let new = event.new_state;
    Ok(registry.writer().call(move |ctx| {
        let store = ctx.store();
        let snapshot = match write {
            StateWrite::Plain => {
                store.set_state(&name, new)?;
                None
            }
            StateWrite::IfPrior(prior) => {
                store.set_state_if_prior(&name, prior, new)?;
                None
            }
            StateWrite::SnapshotBacking {
                expected_prior,
                expected_backing,
            } => store.set_state_snapshotting_memory_backing(
                &name,
                expected_prior,
                new,
                expected_backing,
            )?,
        };
        store.append_transition(&event)?;
        let append = store.append_outbox(&EngineEvent::Transition(event), OUTBOX_MAX_BYTES)?;
        Ok((snapshot, append))
    })?)
}
