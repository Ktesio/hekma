//! The AD-14 event bus (story 7-2, FR-33 / AD-14) — the engine's subscription
//! surface: one bounded broadcast channel fed at the SAME commit points where
//! the event logs are appended, handing out [`broadcast::Receiver`]s over a
//! wrapping [`EngineEvent`].
//!
//! ## The contract (what a subscriber may rely on)
//!
//! * **Committed truth only.** Every publish happens immediately AFTER the
//!   durable append/commit succeeds — a transition after its
//!   `instance.log` append, a breach after its `breaches.log` append, a usage
//!   update after its ledger row commits. A subscriber NEVER sees an event
//!   whose durable record failed to land, and never sees one BEFORE its
//!   durable record exists.
//! * **Publish order == commit order (per-instance FIFO).** Every commit point
//!   runs while the caller holds the supervisor lock, so publishes are
//!   serialized in exactly the durable append order. A subscriber observes one
//!   instance's events in the same order its `instance.log` /
//!   `breaches.log` / ledger record them — FIFO in commit order (the channel
//!   order is in fact the GLOBAL commit order, a strictly stronger property).
//!   This ordering is **caller-enforced**: the bus does not serialize by
//!   itself — the guarantee holds because every current publish site runs
//!   under the supervisor lock, and each site carries a pointer comment naming
//!   that obligation. A future publish from OUTSIDE the lock would silently
//!   break the global-order guarantee.
//! * **Versioned payloads, verbatim.** [`EngineEvent`] is a thin
//!   `kind`-tagged wrapper over the EXISTING AD-14 structs —
//!   [`TransitionEvent`], [`BudgetBreachEvent`], and [`UsageUpdateEvent`]
//!   (which carries the committed [`UsageEvent`]) — with NO new schema
//!   vocabulary. Each payload stamps its own `schema_version`
//!   ([`EVENT_SCHEMA_VERSION`] / [`BUDGET_SCHEMA_VERSION`] /
//!   [`USAGE_SCHEMA_VERSION`]); the wrapper deliberately does NOT duplicate a
//!   version. Serde-derived so a future wire surface can round-trip the same
//!   shapes the query APIs (`transition_events` / `budget_breach_events`)
//!   already return — one event schema, two consumers (AD-14).
//! * **Slow subscribers cannot stall supervision.** The channel is bounded at
//!   [`EVENT_BUS_CAPACITY`] and `broadcast::Sender::send` is synchronous and
//!   non-blocking by construction — a publish NEVER awaits subscriber
//!   capacity, so a stalled receiver cannot back-pressure a transition, a
//!   breach record, or usage ingestion. A receiver that falls more than
//!   [`EVENT_BUS_CAPACITY`] events behind observes
//!   [`broadcast::error::RecvError::Lagged`]`(`n`)` for the `n` dropped
//!   events and then KEEPS WORKING, resynced at the current tail (dropped
//!   events remain in the durable logs — the query APIs read them back).
//!   This is the documented policy, not a failure mode: a lagging host
//!   re-syncs, and what it missed is never lost.
//! * **Publishing failures cannot fail supervision.** `send` returns `Err`
//!   only when there are no receivers at all; the bus swallows that by design
//!   (an unsubscribed engine pays one cheap no-op per event, nothing more).
//!
//! ## Crash-window delivery (honest at-most-once semantics)
//!
//! The durable append and the bus publish are two separate steps, in that
//! order. A process crash in the window between them loses that ONE event
//! from the bus — delivery is at-most-once in the crash window. The durable
//! logs stay COMPLETE either way, and since story 10-3 the remedy is ONE
//! call: [`Engine::resync_events`](crate::Engine::resync_events) /
//! [`Blocking::resync_events`](crate::Blocking::resync_events) backfill the
//! committed records as [`EngineEvent`]s past a
//! [`ResyncCursor`](super::resync::ResyncCursor) (the contract: resync
//! FIRST, then subscribe — see the `domain::resync` module for the ordering
//! and torn-tail tolerance details). The query APIs
//! ([`TransitionEvent`]s via `Engine::transition_events`,
//! [`BudgetBreachEvent`]s via `Engine::budget_breach_events`, ledger reads)
//! remain the narrower per-family reads underneath. The at-most-once WINDOW
//! itself is the documented policy, not a failure mode: closing it in-band
//! would need publish-before-append (violating the committed-truth
//! guarantee) or a durable per-subscriber cursor (a persistence surface the
//! 7-2 review explicitly rejected).
//!
//! ## Boundary (what this is NOT)
//!
//! No wire/HTTP surface, no persistence change, no new event kinds, no `kt`
//! CLI surface. The bus is purely ADDITIVE to the existing query APIs —
//! `Engine::transition_events` / `Engine::budget_breach_events` keep reading
//! the durable logs untouched.

use serde::{Deserialize, Serialize};

use super::event::{BudgetBreachEvent, TransitionEvent};
use super::usage::UsageUpdateEvent;

/// Re-exported so a Host can name the receiver/error types without taking a
/// direct `tokio` dependency (`hekma_engine::broadcast::Receiver<EngineEvent>`).
pub use tokio::sync::broadcast;

/// The bounded capacity of the engine event bus (story 7-2).
///
/// This is the SHARED channel's ring capacity, per engine — not a per-receiver
/// quota. Every receiver's retention window is a view of that one ring, so a
/// receiver retains at most this many of the most recent events no matter how
/// many subscribers exist. 1024 comfortably exceeds any single test Run's
/// event count (the UJ-3 flow produces a dozen), so a normally-draining
/// subscriber never lags in practice. The bound is the documented memory
/// policy: the ring allocates one slot per event, so the worst case is 1024
/// small versioned structs per engine — a flat, known ceiling, never
/// unbounded queue growth. A receiver that falls further behind stops
/// retaining history and reports [`broadcast::error::RecvError::Lagged`]
/// instead (see the module docs).
pub const EVENT_BUS_CAPACITY: usize = 1024;

/// One event on the engine's subscription bus (story 7-2) — the `kind`-tagged
/// wrapper over the EXISTING AD-14 payload structs, verbatim.
///
/// The wrapper adds NO new wire vocabulary: each variant carries one of the
/// already-frozen, already-versioned structs ([`TransitionEvent`],
/// [`BudgetBreachEvent`], [`UsageUpdateEvent`]); the committed
/// [`UsageEvent`](super::usage::UsageEvent) rides inside its
/// [`UsageUpdateEvent`] envelope exactly as the ledger-commit choke point
/// builds it. `schema_version` lives inside each payload already — the
/// wrapper does not duplicate it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineEvent {
    /// A lifecycle transition, published right after its `instance.log`
    /// append succeeded.
    Transition(TransitionEvent),
    /// A Token/Dollar budget breach, published right after its
    /// `breaches.log` append succeeded.
    BudgetBreach(BudgetBreachEvent),
    /// A committed usage measurement (the AD-14 wire struct over the ledger
    /// row), published right after the row committed.
    UsageUpdate(UsageUpdateEvent),
}

/// The in-supervisor handle to the event bus: a cloneable [`broadcast::Sender`]
/// plus the subscribe seam (story 7-2).
///
/// The [`Supervisor`](super::supervisor::Supervisor) owns one clone and
/// publishes at the three commit points; the
/// [`Engine`](crate::Engine) holds another so `subscribe()` never has to take
/// the supervisor lock. `broadcast::Sender::clone` shares the same channel, so
/// receivers handed out from either side see the same stream.
#[derive(Clone, Debug)]
pub(crate) struct EventBus {
    tx: broadcast::Sender<EngineEvent>,
}

impl EventBus {
    /// Build a bus bounded at [`EVENT_BUS_CAPACITY`].
    pub(crate) fn new() -> Self {
        Self {
            tx: broadcast::channel(EVENT_BUS_CAPACITY).0,
        }
    }

    /// Publish one event to every current subscriber — never blocks, never
    /// fails supervision.
    ///
    /// `send` errors ONLY when no receiver exists (an unsubscribed engine);
    /// that is the expected common case and is swallowed by design. A publish
    /// is called exclusively from the supervisor's commit points, AFTER the
    /// durable append succeeded.
    pub(crate) fn publish(&self, event: EngineEvent) {
        // A send with no receivers is an Err that MUST be ignored — a publish
        // failure can never fail supervision (story 7-2 design).
        let _ = self.tx.send(event);
    }

    /// Hand out a fresh receiver positioned at the CURRENT tail (it sees only
    /// events published after this call — the standard broadcast semantics).
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::super::event::TransitionCause;
    use super::*;

    fn sample_transition(seq: u64) -> EngineEvent {
        EngineEvent::Transition(TransitionEvent::new(
            format!("probe-{seq}"),
            super::super::LifecycleState::Running,
            super::super::LifecycleState::Stopped,
            TransitionCause::StopGraceful,
            "2026-09-09T00:00:00Z",
        ))
    }

    #[test]
    fn publish_with_no_subscribers_is_swallowed() {
        // The common engine lifetime has zero subscribers: a publish must be a
        // cheap no-op (send's no-receiver Err swallowed), never a panic or an
        // error surfaced into supervision.
        let bus = EventBus::new();
        bus.publish(sample_transition(0));
        bus.publish(sample_transition(1));
    }

    #[test]
    fn subscribers_fan_out_independently_and_fifo() {
        // Two receivers handed out before the publishes both see the FULL
        // sequence, in publish order, independently of each other's progress.
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        for seq in 0..3u64 {
            bus.publish(sample_transition(seq));
        }
        for rx in [&mut rx1, &mut rx2] {
            for seq in 0..3u64 {
                let got = rx.try_recv().expect("the fanned-out event is buffered");
                assert_eq!(got, sample_transition(seq), "publish order is FIFO");
            }
            assert!(rx.try_recv().is_err(), "exactly the published events");
        }
    }

    #[test]
    fn lagged_receiver_sees_lagged_then_resyncs_at_the_tail() {
        // The documented slow-subscriber policy: a receiver stalled past
        // EVENT_BUS_CAPACITY loses the oldest events, observes Lagged(n)
        // naming the drop count, and then KEEPS WORKING — resynced at the
        // current tail, receiving every subsequent publish.
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let extra = 8usize;
        for seq in 0..(EVENT_BUS_CAPACITY + extra) as u64 {
            bus.publish(sample_transition(seq));
        }
        // First recv: the Lagged marker naming the dropped count (tokio
        // reports the loss BEFORE replaying the retained window).
        match rx.try_recv() {
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                assert_eq!(
                    n as usize, extra,
                    "exactly the events past the window dropped"
                );
            }
            other => panic!("expected Lagged, got {other:?}"),
        }
        // The receiver resynced: it now receives the retained window —
        // starting at the capacity offset, in order — and KEEPS receiving NEW
        // publishes afterwards.
        let first = rx.try_recv().expect("the oldest retained event");
        assert_eq!(
            first,
            sample_transition(extra as u64),
            "the retained window starts at the capacity offset"
        );
        for seq in (extra as u64 + 1)..(EVENT_BUS_CAPACITY + extra) as u64 {
            let got = rx.try_recv().expect("retained tail event");
            assert_eq!(got, sample_transition(seq), "tail order preserved");
        }
        assert!(rx.try_recv().is_err(), "drained to the tail");
        bus.publish(sample_transition(u64::MAX));
        let after = rx
            .try_recv()
            .expect("the resynced receiver receives new events");
        assert_eq!(
            after,
            sample_transition(u64::MAX),
            "the lagged receiver keeps working"
        );
    }

    /// Drain a receiver to its tail with `try_recv`, ACCUMULATING any `Lagged`
    /// counts (saturating — a drain can pass through more than one lag burst if
    /// publishes race the drain) and returning `(events, total_dropped)`.
    fn drain_with_lag(rx: &mut broadcast::Receiver<EngineEvent>) -> (Vec<EngineEvent>, u64) {
        let mut events = Vec::new();
        let mut dropped = 0u64;
        loop {
            match rx.try_recv() {
                Ok(event) => events.push(event),
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    dropped = dropped.saturating_add(n);
                }
                Err(broadcast::error::TryRecvError::Empty) => return (events, dropped),
                Err(broadcast::error::TryRecvError::Closed) => {
                    panic!("the receiver closed while its sender is alive")
                }
            }
        }
    }

    #[test]
    fn a_resynced_receiver_lags_again_with_a_correct_second_lagged_count() {
        // BH16: lag → resync → stall past capacity AGAIN. Retention-window
        // arithmetic must produce a correct SECOND Lagged count and a correct
        // second retained window — exactly where an off-by-one in the ring
        // bookkeeping would surface.
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let round = EVENT_BUS_CAPACITY as u64 + 10;

        // Round 1: exceed the capacity; the receiver (never drained) drops 10.
        for seq in 0..round {
            bus.publish(sample_transition(seq));
        }
        let next = round;
        let (events, dropped) = drain_with_lag(&mut rx);
        assert_eq!(dropped, 10, "round 1 drops exactly the capacity overflow");
        assert_eq!(
            events.len(),
            EVENT_BUS_CAPACITY,
            "round 1 retains the window"
        );
        assert_eq!(
            events[0],
            sample_transition(10),
            "round 1's retained window starts at the offset"
        );

        // Round 2: the resynced receiver stalls past the capacity AGAIN.
        for seq in next..(next + round) {
            bus.publish(sample_transition(seq));
        }
        let (events, dropped) = drain_with_lag(&mut rx);
        assert_eq!(dropped, 10, "round 2 drops exactly its own overflow again");
        assert_eq!(
            events.len(),
            EVENT_BUS_CAPACITY,
            "round 2 retains a full window again"
        );
        assert_eq!(
            events[0],
            sample_transition(round + 10),
            "round 2's retained window starts at ITS offset"
        );
        for (i, event) in events.iter().enumerate() {
            assert_eq!(
                event,
                &sample_transition(round + 10 + i as u64),
                "round 2's window order is exact at {i}"
            );
        }

        // And the twice-resynced receiver still keeps working.
        bus.publish(sample_transition(u64::MAX));
        let (events, dropped) = drain_with_lag(&mut rx);
        assert_eq!(dropped, 0, "no third lag");
        assert_eq!(events, vec![sample_transition(u64::MAX)]);
    }

    #[test]
    fn wrapper_round_trips_each_kind_with_stable_tags() {
        // The wire shape: a kind-tagged envelope whose payloads are the AD-14
        // structs verbatim (schema_version stamps ride inside the payloads;
        // the wrapper adds only `kind`). Each tag is pinned so a future wire
        // surface cannot silently rename it.
        let transition = TransitionEvent::new(
            "probe",
            super::super::LifecycleState::Registered,
            super::super::LifecycleState::Starting,
            TransitionCause::Command {
                command: "start".to_string(),
            },
            "2026-09-09T00:00:00Z",
        );
        let breach = BudgetBreachEvent::new(
            "probe",
            "run-1",
            super::super::BreachScope::Cumulative,
            90,
            90,
            super::super::BreachAction::Pause,
            "self-reported",
            "2026-09-09T00:00:00Z",
        );
        let usage = UsageUpdateEvent::new(super::super::UsageEvent {
            instance: "probe".to_string(),
            run_id: super::super::RunId::from_wire("run-1"),
            input_tokens: 10,
            output_tokens: 20,
            cached_tokens: Some(0),
            metering_source: "self-reported".to_string(),
            sequence: 0,
            occurred_at: "2026-09-09T00:00:00Z".to_string(),
        });
        let cases = [
            (EngineEvent::Transition(transition), "transition"),
            (EngineEvent::BudgetBreach(breach), "budget_breach"),
            (EngineEvent::UsageUpdate(usage), "usage_update"),
        ];
        for (event, tag) in cases {
            let value = serde_json::to_value(&event).expect("serialize the wrapper");
            assert_eq!(
                value.get("kind").and_then(|k| k.as_str()),
                Some(tag),
                "the kind tag is stable: {value}"
            );
            let back: EngineEvent = serde_json::from_value(value).expect("the wrapper round-trips");
            assert_eq!(back, event, "payloads survive the round trip verbatim");
        }
    }
}
