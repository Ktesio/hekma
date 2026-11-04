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
use crate::domain::runs;
use crate::domain::runs::ops::{CommittedRunEvent, RunFate};
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

/// What a transition job committed: the memory-backing snapshot (the
/// `→ starting` shape), the transition's outbox row, and the Run events its
/// projection caused (story 21-2), in commit order — or, when the Run
/// projection itself failed, why (the transition still committed).
pub(super) struct CommittedTransition {
    pub snapshot: Option<MemoryBacking>,
    pub append: OutboxAppend,
    pub run_events: Vec<CommittedRunEvent>,
    pub projection_failed: Option<String>,
}

/// Commit `event`'s state write, event row and outbox row in one writer job,
/// with the transition's projection onto the instance's Run (story 21-2:
/// Run Status, Anomalies and the in-process cascade commit in the same
/// transaction — the Supervisor never takes a Run lock).
pub(super) fn commit_transition(
    registry: &Registry,
    name: &InstanceName,
    write: StateWrite,
    event: &TransitionEvent,
    fate: RunFate,
) -> Result<CommittedTransition, RegistryError> {
    let name = name.clone();
    let event = event.clone();
    let new = event.new_state;
    let publisher = std::sync::Arc::clone(registry.run_publisher());
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
        let append =
            store.append_outbox(&EngineEvent::Transition(event.clone()), OUTBOX_MAX_BYTES)?;
        // The Run projection runs in its own savepoint: Run bookkeeping
        // never blocks a lifecycle transition (graceful degradation) — a
        // failure rolls back only the projection and is surfaced.
        let cause = cause_text(&event.cause);
        let projected = store.in_savepoint("run_projection", || {
            let changes =
                runs::ops::project_transition(store, &name, new, fate, &cause, &event.at)?;
            runs::ops::commit(ctx, &publisher, changes)
        });
        let (run_events, projection_failed) = match projected {
            Ok(events) => (events, None),
            Err(err) => (Vec::new(), Some(err.to_string())),
        };
        Ok(CommittedTransition {
            snapshot,
            append,
            run_events,
            projection_failed,
        })
    })?)
}

/// A transition cause as a Run's end cause: its kind, plus the command or
/// detail it carries (`crashed: exit code 3`, `command: stop`).
pub(super) fn cause_text(cause: &crate::domain::TransitionCause) -> String {
    let value = serde_json::to_value(cause).unwrap_or_default();
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let extra = ["command", "detail"]
        .into_iter()
        .find_map(|field| value.get(field).and_then(serde_json::Value::as_str));
    match extra {
        Some(extra) => format!("{kind}: {extra}"),
        None => kind,
    }
}
