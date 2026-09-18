//! Story 10-3 (FR-33 / AD-14): the event-bus RESYNC acceptance suite — the
//! host-facing remedy for the bus's documented crash-window at-most-once
//! delivery. `Engine::resync_events` / `Blocking::resync_events` backfill an
//! instance's COMMITTED event records (transitions, breaches, ledger rows) as
//! the exact [`EngineEvent`] payloads the live bus delivers.
//!
//! ## The acceptance families
//!
//! 1. **Backfill equals the committed logs exactly (the crash-window
//!    simulation)** — the whole UJ-3 mini-flow (the SHARED `uj3` fixture)
//!    commits while NO subscriber exists (the harshest miss: every publish
//!    landed unheard, exactly as a crash window strands them), then ONE
//!    `resync_events` call returns precisely those events: per family, the
//!    backfill equals the durable record EXACTLY (`instance.log` via the
//!    query API, `breaches.log` likewise, the ledger via the shared
//!    `committed_usage_rows` projection) in that family's commit order, each
//!    payload round-tripping serde with its schema stamp; and a re-resync
//!    with the returned cursor is EMPTY (idempotent — nothing re-delivered).
//! 2. **Backfill-then-subscribe: no duplicates, no gaps** — phase one
//!    commits with no subscriber (the missed window), the host backfills,
//!    THEN subscribes live, phase two commits. For every family:
//!    `backfilled ++ received` equals the committed record EXACTLY — the
//!    backfill is precisely the prefix, the live stream precisely the
//!    suffix, so the union window has no duplicate and no gap (the
//!    resync-first ordering — one of the two documented orders, the gap-free
//!    one only across a quiescent agent — held end to end).
//! 3. **Cursor continuation** — a resync consumed mid-stream, the cursor
//!    passed back after MORE commits, returns exactly the new events: the
//!    incremental heal a lagging/crashed host needs, never a re-delivery of
//!    the consumed prefix. A cursor past a family's committed count (a
//!    truncated/rotated log) is a TYPED ERROR naming the family and both
//!    counts — never a silent clamp, which would re-deliver or silently skip.
//! 4. **Honest edges** — an unregistered name fails `NotFound` (a mistyped
//!    name must not read as a silent empty backfill), a malformed name fails
//!    `InvalidName`, and a torn trailing append (the very crash this helper
//!    heals) is skipped WITH the skip surfaced (`torn_tail_skipped`): the
//!    good prefix returns, and the host knows a record may be missing.
//! 5. **Wire pins** — `ResyncCursor`/`ResyncBatch` serialize as snake_case
//!    and round-trip (the documented host-persistence contract), with
//!    `torn_tail_skipped` additive (`#[serde(default)]`: an archived batch
//!    without it still loads).
//!
//! ## Determinism posture (the house style, shared with `events_subscription.rs`)
//!
//! No wall-clock sleeps against side effects: every flow-driving facade call
//! is synchronous and its publishes complete UNDER the supervisor lock before
//! the call returns, so after a committed-state wait (`uj3::wait_for_state`
//! / the ledger row count) plus ONE supervisor-lock-taking read (the
//! `fleet()` barrier) any attached receiver's `try_recv`-until-empty drain is
//! exact, never racy. The resync itself is a synchronous committed-state
//! read — after the stop leg (terminal state) the records are frozen.

use std::time::Duration;

use hekma_conformance::test_support::{self, ManifestFixture};
use hekma_conformance::uj3;
use hekma_engine::{
    AdapterRef, BudgetBreachEvent, Engine, EngineError, EngineEvent, LifecycleState, ResyncBatch,
    ResyncCursor, TransitionEvent, UsageUpdateEvent, BUDGET_SCHEMA_VERSION, EVENT_SCHEMA_VERSION,
    USAGE_SCHEMA_VERSION,
};
use tempfile::TempDir;

/// The supervisor-lock barrier (the `events_subscription.rs` helper): a
/// fleet read takes the supervisor mutex, so once it returns every
/// in-flight publisher has fully finished — commit AND publish.
fn barrier(facade: &hekma_engine::Blocking<'_>) {
    facade.fleet().expect("barrier fleet read");
}

/// Per-family projection of a resync/live stream.
fn transitions_of(events: &[EngineEvent]) -> Vec<TransitionEvent> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Transition(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

fn breaches_of(events: &[EngineEvent]) -> Vec<BudgetBreachEvent> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::BudgetBreach(b) => Some(b.clone()),
            _ => None,
        })
        .collect()
}

fn usage_of(events: &[EngineEvent]) -> Vec<UsageUpdateEvent> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::UsageUpdate(u) => Some(u.clone()),
            _ => None,
        })
        .collect()
}

/// Payload FIDELITY: every event round-trips serde and stamps its own
/// schema version (the same validation `events_subscription.rs` applies to
/// the live stream — the backfill must be indistinguishable).
fn assert_payload_validates(event: &EngineEvent) {
    let json = serde_json::to_string(event).expect("the wrapper serializes");
    let back: EngineEvent = serde_json::from_str(&json).expect("the wrapper round-trips");
    assert_eq!(
        &back, event,
        "payloads survive the wire round trip verbatim"
    );
    match event {
        EngineEvent::Transition(e) => assert_eq!(e.schema_version, EVENT_SCHEMA_VERSION),
        EngineEvent::BudgetBreach(e) => assert_eq!(e.schema_version, BUDGET_SCHEMA_VERSION),
        EngineEvent::UsageUpdate(e) => assert_eq!(e.schema_version, USAGE_SCHEMA_VERSION),
    }
}

// ---------------------------------------------------------------------------
// Family (1): the backfill equals the committed logs exactly
// (the crash-window simulation: committed, never delivered)
// ---------------------------------------------------------------------------

#[test]
fn resync_returns_exactly_the_committed_logs_after_the_crash_window() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let manifest_dir = uj3::write_flow_manifest(manifest.path());

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    let facade = engine.blocking();

    // NO subscriber anywhere: the whole flow commits unpublished — the
    // crash-window miss in its harshest form (every publish landed unheard).
    facade
        .register_with_adapter(uj3::FLOW_INSTANCE, &AdapterRef::Manifest(manifest_dir))
        .expect("register the flow's agent");
    for (key, value) in uj3::flow_config_pairs() {
        facade
            .set_config(uj3::FLOW_INSTANCE, key, value)
            .unwrap_or_else(|e| panic!("set_config {key}={value} failed: {e}"));
    }
    facade.start(uj3::FLOW_INSTANCE).expect("start");
    uj3::wait_for_state(
        state.path(),
        uj3::FLOW_INSTANCE,
        LifecycleState::Paused,
        uj3::STATE_POLL_BUDGET,
    );
    uj3::stop_resilient(&facade, uj3::FLOW_INSTANCE, uj3::STOP_WINDOW);
    uj3::wait_for_state(
        state.path(),
        uj3::FLOW_INSTANCE,
        LifecycleState::Stopped,
        uj3::STATE_POLL_BUDGET,
    );
    barrier(&facade);

    // ONE call heals the window.
    let batch = facade
        .resync_events(uj3::FLOW_INSTANCE, hekma_engine::ResyncCursor::START)
        .expect("the resync backfill");
    assert!(!batch.events.is_empty(), "the committed flow backfills");

    // EVERY family backfills EXACTLY: the transitions equal the committed
    // `instance.log` (via the query API), the breaches equal `breaches.log`,
    // the usage payloads equal the committed ledger rows field-for-field —
    // each in that family's own commit order.
    let committed_transitions = facade
        .transition_events(uj3::FLOW_INSTANCE)
        .expect("committed transition log read");
    let committed_breaches = facade
        .budget_breach_events(uj3::FLOW_INSTANCE)
        .expect("committed breach log read");
    let committed_rows = uj3::committed_usage_rows(state.path(), uj3::FLOW_INSTANCE);
    assert!(
        !committed_transitions.is_empty() && !committed_rows.is_empty(),
        "the flow committed transitions and usage to compare against"
    );
    assert_eq!(
        transitions_of(&batch.events),
        committed_transitions,
        "the backfilled transitions equal the durable log exactly, in order"
    );
    assert_eq!(
        breaches_of(&batch.events),
        committed_breaches,
        "the backfilled breaches equal the durable log exactly, in order"
    );
    let backfilled_usage: Vec<_> = usage_of(&batch.events)
        .iter()
        .map(uj3::usage_from_payload)
        .collect();
    assert_eq!(
        backfilled_usage, committed_rows,
        "the backfilled usage equals the committed ledger rows field-for-field, in order"
    );
    // Nothing extra: the batch is exactly the union of the three families.
    assert_eq!(
        batch.events.len(),
        committed_transitions.len() + committed_breaches.len() + committed_rows.len(),
        "no event is fabricated and none dropped across the three families"
    );
    for event in &batch.events {
        assert_payload_validates(event);
    }
    assert!(
        !batch.torn_tail_skipped,
        "a clean flow's backfill surfaces no torn-tail skip"
    );
    // The breach family carries the SHARED uj3 shape assertions (both
    // dimensions, pinned numbers) — the backfill is the real committed truth.
    uj3::assert_flow_breaches(&breaches_of(&batch.events));

    // Idempotence: a re-resync from the returned cursor delivers NOTHING —
    // the host is caught up, and nothing is re-delivered.
    let again = facade
        .resync_events(uj3::FLOW_INSTANCE, batch.cursor)
        .expect("the idempotent re-resync");
    assert!(
        again.events.is_empty(),
        "a caught-up cursor re-delivers nothing: {:?}",
        again.events
    );
    assert_eq!(again.cursor, batch.cursor, "the cursor holds at the tail");
}

// ---------------------------------------------------------------------------
// Family (2): backfill FIRST, subscribe SECOND (the resync-first ordering,
// gap-free here because the agent is quiescent across the seam) — no
// duplicates, no gaps
// ---------------------------------------------------------------------------

#[test]
fn backfill_then_subscribe_delivers_the_committed_window_with_no_duplicates_and_no_gaps() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let manifest_dir = uj3::write_flow_manifest(manifest.path());

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    let facade = engine.blocking();

    // ---- Phase 1 (the missed window): committed with no subscriber. ----
    facade
        .register_with_adapter(uj3::FLOW_INSTANCE, &AdapterRef::Manifest(manifest_dir))
        .expect("register the flow's agent");
    for (key, value) in uj3::flow_config_pairs() {
        facade
            .set_config(uj3::FLOW_INSTANCE, key, value)
            .unwrap_or_else(|e| panic!("set_config {key}={value} failed: {e}"));
    }
    facade.start(uj3::FLOW_INSTANCE).expect("start");
    uj3::wait_for_state(
        state.path(),
        uj3::FLOW_INSTANCE,
        LifecycleState::Paused,
        uj3::STATE_POLL_BUDGET,
    );

    // The host backfills the window…
    let backfill = facade
        .resync_events(uj3::FLOW_INSTANCE, hekma_engine::ResyncCursor::START)
        .expect("the window backfill");
    assert!(!backfill.events.is_empty(), "the missed window backfills");

    // …THEN subscribes live (the resync-first ordering — clean continuity
    // here because the flow is quiescent across the seam).
    let mut sub = facade.subscribe();

    // ---- Phase 2 (the live window): resume + stop commit and deliver. ----
    facade.resume(uj3::FLOW_INSTANCE).expect("resume");
    facade
        .stop(uj3::FLOW_INSTANCE, Some(uj3::STOP_WINDOW))
        .expect("stop");
    uj3::wait_for_state(
        state.path(),
        uj3::FLOW_INSTANCE,
        LifecycleState::Stopped,
        uj3::STATE_POLL_BUDGET,
    );
    barrier(&facade);
    let (received, lagged) = test_support::drain_subscription(&mut sub);
    assert!(lagged.is_none(), "a mini-flow never lags");
    assert!(
        !received.is_empty(),
        "the live stream delivered the phase-two commits"
    );

    // THE continuity assertion, per family: backfilled ++ received equals
    // the committed record EXACTLY. The backfill is precisely the prefix,
    // the live stream precisely the suffix — no duplicate at the seam, no
    // gap anywhere in the window.
    let committed_transitions = facade
        .transition_events(uj3::FLOW_INSTANCE)
        .expect("committed transition log read");
    assert_eq!(
        transitions_of(&backfill.events)
            .iter()
            .chain(transitions_of(&received).iter())
            .cloned()
            .collect::<Vec<TransitionEvent>>(),
        committed_transitions,
        "transitions: backfilled prefix + live suffix == the durable log exactly"
    );
    assert_eq!(
        breaches_of(&backfill.events)
            .iter()
            .chain(breaches_of(&received).iter())
            .cloned()
            .collect::<Vec<BudgetBreachEvent>>(),
        facade
            .budget_breach_events(uj3::FLOW_INSTANCE)
            .expect("committed breach log read"),
        "breaches: backfilled prefix + live suffix == the durable log exactly"
    );
    let whole_usage: Vec<_> = usage_of(&backfill.events)
        .iter()
        .chain(usage_of(&received).iter())
        .map(uj3::usage_from_payload)
        .collect();
    assert_eq!(
        whole_usage,
        uj3::committed_usage_rows(state.path(), uj3::FLOW_INSTANCE),
        "usage: backfilled prefix + live suffix == the committed ledger exactly"
    );
}

// ---------------------------------------------------------------------------
// Family (3): the cursor continues from where the host left off
// ---------------------------------------------------------------------------

#[test]
fn a_resync_cursor_continues_from_where_the_host_left_off() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    ManifestFixture::lingering("resync-cursor").write(manifest.path());

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    let facade = engine.blocking();

    facade
        .register_with_adapter(
            "resync-cursor",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade.start("resync-cursor").unwrap();
    uj3::wait_for_state(
        state.path(),
        "resync-cursor",
        LifecycleState::Running,
        uj3::STATE_POLL_BUDGET,
    );
    barrier(&facade);

    // Consume the start edges (registered→starting, starting→running).
    let first = facade
        .resync_events("resync-cursor", hekma_engine::ResyncCursor::START)
        .expect("the first resync");
    assert_eq!(first.cursor.transitions, 2, "the two start edges");
    assert_eq!(
        first.cursor.usage, 0,
        "the heartbeat fixture emits no usage — the ledger position stays at zero"
    );

    // MORE commits land…
    facade.pause("resync-cursor").unwrap();
    facade.resume("resync-cursor").unwrap();
    barrier(&facade);

    // …and the cursor returns EXACTLY the new events — the consumed prefix
    // is never re-delivered.
    let next = facade
        .resync_events("resync-cursor", first.cursor)
        .expect("the incremental resync");
    assert_eq!(next.events.len(), 2, "exactly the pause + resume edges");
    let shapes: Vec<LifecycleState> = transitions_of(&next.events)
        .iter()
        .map(|t| t.new_state)
        .collect();
    assert_eq!(
        shapes,
        vec![LifecycleState::Paused, LifecycleState::Running],
        "the incremental batch is the new tail, in commit order"
    );

    // A cursor past a family's committed count is a TYPED ERROR, never a
    // silent clamp: a count below the cursor means the log was truncated or
    // rotated away, and clamping would either re-deliver from zero or
    // silently skip records — both violate the never-re-deliver contract.
    // The error names the family and BOTH counts, so resetting the cursor
    // (deliberately, to ResyncCursor::START) is a conscious host decision.
    let mut overrun = first.cursor;
    overrun.transitions += 1_000;
    let err = facade
        .resync_events("resync-cursor", overrun)
        .expect_err("a cursor past a family's committed count fails, never clamps");
    assert!(
        matches!(&err, EngineError::Log { detail, .. }
            if detail.contains("transition") && detail.contains("(1002)") && detail.contains("(4)")),
        "the error names the family and both counts: {err:?}"
    );

    uj3::stop_resilient(&facade, "resync-cursor", Duration::from_secs(5));
}

// ---------------------------------------------------------------------------
// Family (4): honest edges — name errors and the torn trailing append
// ---------------------------------------------------------------------------

#[test]
fn resync_surfaces_the_expected_error_arms_and_tolerates_a_torn_tail() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    ManifestFixture::lingering("resync-edges").write(manifest.path());

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    let facade = engine.blocking();

    // A malformed name fails validation before any read.
    assert!(matches!(
        facade
            .resync_events("Bad Name", hekma_engine::ResyncCursor::START)
            .unwrap_err(),
        EngineError::InvalidName { name, .. } if name == "Bad Name"
    ));
    // An unregistered (but well-formed) name fails NotFound — a mistyped
    // name must not read as a silent empty backfill (the read_agent_log
    // precedent).
    assert!(matches!(
        facade
            .resync_events("never-registered", hekma_engine::ResyncCursor::START)
            .unwrap_err(),
        EngineError::NotFound { name } if name == "never-registered"
    ));

    // The torn-trailing-append case, end to end through the facade: drive a
    // REAL lifecycle (four committed transitions), then tear the log's
    // trailing line the way the crash this helper heals would — the backfill
    // returns exactly the good prefix, never a failed recovery.
    facade
        .register_with_adapter(
            "resync-edges",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade.start("resync-edges").unwrap();
    uj3::wait_for_state(
        state.path(),
        "resync-edges",
        LifecycleState::Running,
        uj3::STATE_POLL_BUDGET,
    );
    uj3::stop_resilient(&facade, "resync-edges", Duration::from_secs(5));
    barrier(&facade);
    let committed = facade
        .transition_events("resync-edges")
        .expect("the committed transitions");
    assert_eq!(committed.len(), 4, "start + stop edges committed");

    let log = state
        .path()
        .join("agents")
        .join("resync-edges")
        .join("logs")
        .join("instance.log");
    let existing = std::fs::read_to_string(&log).expect("the transition log exists");
    std::fs::write(&log, format!("{existing}{{\"schema_version\":1,\"inst"))
        .expect("tear the tail");

    let batch = facade
        .resync_events("resync-edges", hekma_engine::ResyncCursor::START)
        .expect("the torn tail is skipped, not a failed recovery");
    assert_eq!(
        transitions_of(&batch.events),
        committed,
        "exactly the good prefix backfills"
    );
    assert!(
        batch.torn_tail_skipped,
        "the torn-tail skip is SURFACED on the batch — the engine's next append fuses onto the \
         torn fragment, so the host must know a record may be missing"
    );

    // The tolerance is bounded: a malformed INTERIOR line (a bad line with a
    // good line AFTER it — not a trailing race) is the wrong file or a
    // corrupting engine, and the typed error surfaces. Proven through the
    // FACADE for BOTH log families, so both error-mapping arms are covered
    // end to end.
    let good_line = serde_json::to_string(&committed[0]).unwrap();
    std::fs::write(&log, format!("{{\"not\":\"a transition\"}}\n{good_line}\n"))
        .expect("corrupt the transition log's interior");
    let err = facade
        .resync_events("resync-edges", hekma_engine::ResyncCursor::START)
        .unwrap_err();
    assert!(matches!(err, EngineError::Log { .. }), "{err:?}");

    let good_breach = serde_json::json!({
        "schema_version": 1, "instance": "resync-edges", "run_id": "run-1",
        "scope": "cumulative", "dimension": "tokens", "limit": 90,
        "observed": 90, "action": "pause", "metering_source": "self-reported",
        "at": "2026-09-10T00:00:00Z"
    });
    let breach_log = state
        .path()
        .join("agents")
        .join("resync-edges")
        .join("logs")
        .join("breaches.log");
    std::fs::write(&breach_log, format!("{{\"nope\":1}}\n{good_breach}\n"))
        .expect("corrupt the breach log's interior");
    let err = facade
        .resync_events("resync-edges", hekma_engine::ResyncCursor::START)
        .unwrap_err();
    assert!(matches!(err, EngineError::Log { .. }), "{err:?}");
}

// ---------------------------------------------------------------------------
// Family (5): the wire pins — the host-persistence contract, literally
// ---------------------------------------------------------------------------

#[test]
fn resync_wire_structs_round_trip_snake_case_beside_the_payload_validator() {
    // The documented host-persistence contract: `ResyncCursor`/`ResyncBatch`
    // are serde-derived snake_case so a host can persist its cursor across
    // its own restarts. Pinned LITERALLY (the exact wire bytes a host's
    // saved file carries) AND by round-trip equality, beside
    // `assert_payload_validates` (the payload-shape validator above).
    let cursor = ResyncCursor {
        transitions: 3,
        breaches: 1,
        usage: 7,
    };
    let json = serde_json::to_string(&cursor).expect("the cursor serializes");
    assert_eq!(
        json, r#"{"transitions":3,"breaches":1,"usage":7}"#,
        "the cursor wire shape is snake_case, field-complete, in declaration order"
    );
    let back: ResyncCursor = serde_json::from_str(&json).expect("the cursor round-trips");
    assert_eq!(back, cursor);

    // The batch: `torn_tail_skipped` is ADDITIVE on the wire
    // (`#[serde(default)]`, the report family's policy) — a current batch
    // round-trips carrying the flag, and an ARCHIVED batch without the
    // field still deserializes (as `false`).
    let batch = ResyncBatch {
        events: Vec::new(),
        cursor,
        torn_tail_skipped: true,
    };
    let json = serde_json::to_string(&batch).expect("the batch serializes");
    assert!(
        json.contains(r#""torn_tail_skipped":true"#),
        "the additive field rides the wire when set: {json}"
    );
    assert!(
        json.contains(r#""events":[]"#) && json.contains(r#""cursor""#),
        "the batch carries events + cursor snake_case keys: {json}"
    );
    let back: ResyncBatch = serde_json::from_str(&json).expect("the batch round-trips");
    assert_eq!(back, batch);

    let legacy = r#"{"events":[],"cursor":{"transitions":3,"breaches":1,"usage":7}}"#;
    let back: ResyncBatch = serde_json::from_str(legacy).expect("an archived batch still loads");
    assert_eq!(
        back,
        ResyncBatch {
            events: Vec::new(),
            cursor,
            torn_tail_skipped: false,
        },
        "a batch produced before the additive field deserializes with the serde default"
    );
}
