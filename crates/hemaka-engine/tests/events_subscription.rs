//! Story 7-2 (FR-33 / AD-14): the event-subscription acceptance suite — a Host
//! OBSERVES the engine through `Engine::subscribe()` / `Blocking::subscribe()`
//! and every payload family arrives in commit order with schema-stable shapes.
//!
//! ## The acceptance families (the spec's test core + the triage hardening)
//!
//! 1. **Flow order + payload validation** — a subscriber attached BEFORE the
//!    UJ-3 mini-flow sees every transition, both breach dimensions, and the
//!    usage updates in publish order (== durable commit order), each payload
//!    round-tripping serde into its versioned AD-14 struct, and the received
//!    usage stream equaling the COMMITTED LEDGER ROWS exactly (ordered).
//!    Driven with the SHARED `hemaka_conformance::uj3` fixture (the same
//!    manifest, config pairs, numbers, and committed-state pollers the 7-1
//!    suites pin), not a reinvented one.
//! 2. **Crash/restart leg** — a crashed-then-restarted instance delivers the
//!    `crashed` and `restarted` transitions on the bus, exactly matching the
//!    committed event log.
//! 3. **Slow subscriber: no stall + Lagged** — a receiver that never drains
//!    cannot stall supervision: well over [`EVENT_BUS_CAPACITY`] events
//!    publish (real pause/resume transitions) while the instance keeps
//!    committing; the stalled receiver then observes `Lagged(n)`, and the
//!    events it retains correspond EXACTLY to the durable log's tail (nothing
//!    committed is ever mis-delivered; the dropped prefix stays readable via
//!    the query APIs). A post-lag publish proves the receiver resynced.
//! 4. **Fan-out** — two subscribers each independently receive the full
//!    sequence.
//! 5. **Per-instance FIFO under interleaving** — facade-driven transitions
//!    from two instances interleave on the one bus in exact call order, and a
//!    second pair of instances with OVERLAPPING usage-emission cadences keeps
//!    per-instance FIFO for the usage family against the committed ledger.
//! 6. **Append-failure silence (VG1)** — with a subscriber attached, a failed
//!    durable append (the log file replaced by a DIRECTORY) publishes NOTHING:
//!    the failed event appears on neither the bus nor the durable log, while
//!    the next successful commit delivers. Proven for the transition append
//!    AND the breach append (a failed breach record still enforces — the pause
//!    transition delivers — but no breach event is ever published).
//! 7. **Replay silence (VG2)** — a replayed usage batch (the `metering.rs`
//!    `--replay-usage` pattern) publishes no duplicate: the received usage
//!    stream equals the committed ledger rows exactly.
//! 8. **Async surface (BH3)** — the raw `Engine::subscribe()` receiver driven
//!    from a real `#[tokio::test]` consumer while a plain thread drives the
//!    flow through the facade: every commit arrives over `recv().await`,
//!    bounded.
//!
//! ## Determinism posture (the house style, shared with `budget.rs`/7-1)
//!
//! No wall-clock sleeps against side effects: every flow-driving facade call
//! is synchronous and its publishes complete UNDER the supervisor lock before
//! the call returns, so after a committed-state wait plus ONE supervisor-lock-
//! taking read (the barrier) the channel provably holds every event published
//! so far — a plain `try_recv`-until-empty drain is then exact, never racy.
//! Waits poll COMMITTED state (`uj3::wait_for_state` / `instance_status` /
//! the ledger row count). The one deliberate sleep (the replay settle) is the
//! proven `metering.rs` pattern for this exact fixture and is cited inline.
//! The crash leg tolerates the production 1s restart backoff exactly like
//! `tests/crash.rs` (a single crash, `--crash-times 1`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hemaka_conformance::test_support::{self, ManifestFixture};
use hemaka_conformance::uj3::{self, committed_usage_rows, usage_from_payload, CommittedUsage};
use hemaka_engine::{
    broadcast, AdapterRef, BreachDimension, Engine, EngineEvent, RestartPolicy, TransitionCause,
    TransitionEvent, BUDGET_SCHEMA_VERSION, EVENT_BUS_CAPACITY, EVENT_SCHEMA_VERSION,
    USAGE_SCHEMA_VERSION,
};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Shared drain/wait helpers
//
// The fixture manifests are the SHARED `hemaka_conformance::test_support`
// presets (story 10-1): `ManifestFixture::lingering` for the cross-OS pause
// families, `crash_once` for the crash/restart leg, `replay_batch` for the
// VG2 leg — no local manifest-TOML builder lives here anymore. The drains
// are the shared `test_support::drain_subscription` (ONE lag-accumulating
// implementation for both receiver forms).
// ---------------------------------------------------------------------------

/// Poll `instance_status` until `pred(state)` holds, bounded (the `crash.rs`
/// helper, shared shape).
fn wait_until_state(
    facade: &hemaka_engine::Blocking<'_>,
    name: &str,
    pred: impl Fn(hemaka_engine::LifecycleState) -> bool,
    within: Duration,
    what: &str,
) -> hemaka_engine::LifecycleState {
    let deadline = Instant::now() + within;
    loop {
        let state = facade
            .instance_status(name)
            .map(|s| s.instance.state)
            .unwrap_or(hemaka_engine::LifecycleState::Registered);
        if pred(state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} (last state: {state})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

// The subscription drain is the SHARED `test_support::drain_subscription`
// (story 10-1) — ONE lag-accumulating `try_recv` implementation for both
// receiver forms; its `Closed` arm is the invariant panic (a subscription
// holds its engine's runtime, so a closed bus mid-test is a violation).

/// The supervisor-lock barrier: a fleet read takes the supervisor mutex, so
/// once it returns, any concurrently-running publisher (the reaper's poll /
/// enforcement pass) has fully finished — commit AND publish. Only needed
/// after committed-state polls that can race an in-flight reaper pass.
fn barrier(facade: &hemaka_engine::Blocking<'_>) {
    facade.fleet().expect("barrier fleet read");
}

/// Validate one received event's envelope: the wrapper round-trips serde and
/// every payload carries its own schema-version stamp. Returns the event back
/// (for further pattern matching by the caller).
fn assert_payload_validates(event: &EngineEvent) {
    let json = serde_json::to_string(event).expect("the wrapper serializes");
    let back: EngineEvent = serde_json::from_str(&json).expect("the wrapper round-trips");
    assert_eq!(
        &back, event,
        "payloads survive the wire round trip verbatim"
    );
    match event {
        EngineEvent::Transition(e) => {
            assert_eq!(e.schema_version, EVENT_SCHEMA_VERSION);
        }
        EngineEvent::BudgetBreach(e) => {
            assert_eq!(e.schema_version, BUDGET_SCHEMA_VERSION);
        }
        EngineEvent::UsageUpdate(e) => {
            assert_eq!(e.schema_version, USAGE_SCHEMA_VERSION);
        }
    }
}

/// The committed transition log, through the facade query API (the durable
/// record the bus must mirror).
fn committed_transitions(facade: &hemaka_engine::Blocking<'_>, name: &str) -> Vec<TransitionEvent> {
    facade
        .transition_events(name)
        .expect("committed transition log read")
}

/// Extract the transition payloads from a received stream.
fn transitions_of(events: &[EngineEvent]) -> Vec<&TransitionEvent> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Transition(t) => Some(t),
            _ => None,
        })
        .collect()
}

/// Extract the usage-update payloads from a received stream.
fn usage_of(events: &[EngineEvent]) -> Vec<hemaka_engine::UsageUpdateEvent> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::UsageUpdate(u) => Some(u.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Committed-ledger reading (the SHARED uj3 readers: a direct read-only
// connection to the same state DB the engine commits to, over the
// engine-published STATE_DB_FILE constant; rowid order IS the commit order).
// The `CommittedUsage` projection + `committed_usage_rows` live in
// `hemaka_conformance::uj3` so this suite and the 7-3 collision test compare
// received streams against the ledger through ONE shape.
// ---------------------------------------------------------------------------

/// The committed ledger ROW COUNT for `name` (the `metering.rs` poll target).
fn usage_row_count(state_dir: &Path, name: &str) -> u64 {
    let conn = rusqlite::Connection::open(state_dir.join(hemaka_engine::paths::STATE_DB_FILE))
        .expect("open state db");
    conn.query_row(
        "SELECT COUNT(*) FROM usage_events e \
         JOIN agent_instances i ON i.id = e.instance_id WHERE i.name = ?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n.max(0) as u64)
    .unwrap_or(0)
}

/// Poll the committed ledger until `name` has at least `expected` rows
/// (deterministic committed state — never a wall-clock guess).
fn wait_for_usage_rows(state_dir: &Path, name: &str, expected: u64, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        let count = usage_row_count(state_dir, name);
        if count >= expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} committed usage rows for '{name}' (have {count})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Payload FIDELITY: the received usage stream must equal the committed ledger
/// rows EXACTLY — one payload per row, field-for-field, in commit order (the
/// projection through the SHARED `uj3::usage_from_payload` shape).
fn assert_usage_matches_rows(
    usage: &[hemaka_engine::UsageUpdateEvent],
    rows: &[CommittedUsage],
    what: &str,
) {
    assert_eq!(
        usage.len(),
        rows.len(),
        "{what}: the bus delivers exactly the committed ledger rows \
         (received {}, committed {})",
        usage.len(),
        rows.len()
    );
    for (u, r) in usage.iter().zip(rows) {
        assert_eq!(
            &usage_from_payload(u),
            r,
            "{what}: the payload must equal the committed row field-for-field"
        );
    }
}

// ---------------------------------------------------------------------------
// Family (a): flow order + payload validation (the UJ-3 mini-flow)
// ---------------------------------------------------------------------------

#[test]
fn subscriber_sees_the_uj3_flow_in_commit_order_with_schema_valid_payloads() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let manifest_dir = uj3::write_flow_manifest(manifest.path());

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    let facade = engine.blocking();

    // Subscribe BEFORE anything publishes: the receiver must see the whole
    // flow, from the first `registered -> starting` edge on.
    let mut sub = facade.subscribe();

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
        hemaka_engine::LifecycleState::Paused,
        uj3::STATE_POLL_BUDGET,
    );
    // Barrier (see `barrier`): the reaper's breaching pass is done publishing.
    barrier(&facade);
    let (phase1, lagged1) = test_support::drain_subscription(&mut sub);
    assert!(
        lagged1.is_none(),
        "a dozen events never exceed the capacity"
    );

    facade
        .stop(uj3::FLOW_INSTANCE, Some(uj3::STOP_WINDOW))
        .expect("stop");
    let (phase2, lagged2) = test_support::drain_subscription(&mut sub);
    assert!(lagged2.is_none());

    let events: Vec<EngineEvent> = phase1.iter().cloned().chain(phase2).collect();
    assert!(!events.is_empty(), "the flow publishes events");
    for event in &events {
        assert_payload_validates(event);
    }
    for event in &events {
        let instance = match event {
            EngineEvent::Transition(e) => e.instance.as_str(),
            EngineEvent::BudgetBreach(e) => e.instance.as_str(),
            EngineEvent::UsageUpdate(e) => e.event.instance.as_str(),
        };
        assert_eq!(
            instance,
            uj3::FLOW_INSTANCE,
            "the bus carries only this flow"
        );
    }

    // THE ordering guarantee: the received transitions equal the committed
    // instance.log EXACTLY — same events, same order (publish order == commit
    // order).
    let committed = committed_transitions(&facade, uj3::FLOW_INSTANCE);
    let received: Vec<TransitionEvent> = transitions_of(&events).into_iter().cloned().collect();
    assert_eq!(
        received, committed,
        "the bus mirrors the durable transition log exactly"
    );

    // The breach payloads equal the committed breach log AND satisfy the
    // SHARED uj3 breach assertions (both dimensions, pinned numbers/shapes).
    let breaches: Vec<hemaka_engine::BudgetBreachEvent> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::BudgetBreach(b) => Some(b.clone()),
            _ => None,
        })
        .collect();
    uj3::assert_flow_breaches(&breaches);
    assert_eq!(
        breaches,
        uj3::read_breach_events(state.path(), uj3::FLOW_INSTANCE),
        "the bus mirrors the durable breach log exactly"
    );

    // The usage payloads: the fixed 10+20 sentinels, strictly increasing
    // per-Run sequences, the pinned schema stamp.
    let usage = usage_of(&events);
    assert!(
        usage.len() as u64 >= uj3::MIN_STOPPED_EVENTS,
        "at least the crossing events arrived: {}",
        usage.len()
    );
    let run_ids: Vec<_> = usage
        .iter()
        .map(|u| u.event.run_id.as_str().to_string())
        .collect();
    assert!(
        run_ids.iter().all(|r| r == &run_ids[0]),
        "one Run spans the flow's usage: {run_ids:?}"
    );
    for (prev, cur) in usage.iter().zip(usage.iter().skip(1)) {
        assert!(
            cur.event.sequence > prev.event.sequence,
            "per-Run usage sequences are strictly increasing (FIFO)"
        );
        assert_eq!(cur.event.total_tokens(), uj3::TOKENS_PER_EVENT);
    }

    // Payload FIDELITY (BH10): the received usage stream equals the ACTUALLY
    // committed usage_events ledger rows exactly — one payload per row,
    // field-for-field, in commit order (rowid order).
    let rows = committed_usage_rows(state.path(), uj3::FLOW_INSTANCE);
    assert_usage_matches_rows(&usage, &rows, "the uj3 flow");

    // The commit-order interleaving inside the pre-stop phase: the first two
    // events are the start transitions; then usage flows; the TOKEN breach
    // lands right after the crossing usage commit (the durable order is
    // usage -> token breach -> pause -> dollar breach); the pause transition
    // follows; then the DOLLAR breach. Any post-pause usage commits (events
    // racing the suspension) come after all of those.
    let first_breach = phase1
        .iter()
        .position(|e| matches!(e, EngineEvent::BudgetBreach(_)))
        .expect("the pre-stop phase carries the breach");
    assert!(
        first_breach >= 2 + uj3::MIN_STOPPED_EVENTS as usize,
        "transitions + usage precede the breach (got breach at {first_breach})"
    );
    assert!(
        phase1[..first_breach]
            .iter()
            .all(|e| matches!(e, EngineEvent::Transition(_) | EngineEvent::UsageUpdate(_))),
        "only transitions and usage precede the breach"
    );
    match &phase1[first_breach] {
        EngineEvent::BudgetBreach(b) => {
            assert_eq!(
                b.dimension,
                BreachDimension::Tokens,
                "the TOKEN breach wins"
            );
        }
        other => panic!("expected the token breach, got {other:?}"),
    }
    match &phase1[first_breach + 1] {
        EngineEvent::Transition(t) => {
            assert_eq!(t.new_state, hemaka_engine::LifecycleState::Paused);
            assert!(
                matches!(t.cause, TransitionCause::BudgetExceeded { .. }),
                "the pause carries the breach cause"
            );
        }
        other => panic!("expected the paused transition, got {other:?}"),
    }
    match &phase1[first_breach + 2] {
        EngineEvent::BudgetBreach(b) => {
            assert_eq!(b.dimension, BreachDimension::Dollars);
        }
        other => panic!("expected the dollar breach, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Family (b): the crash/restart leg
// ---------------------------------------------------------------------------

#[test]
fn subscriber_sees_the_crashed_and_restarted_transitions() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // The crash/restart fixture: the SHARED crash-once preset (story 10-1;
    // the readiness-safe 1500ms delay rationale lives on the preset).
    ManifestFixture::crash_once("sub-crashy", &manifest.path().join("crash-count"))
        .write(manifest.path());

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    let facade = engine.blocking();
    let mut sub = facade.subscribe();

    facade
        .register_with_adapter(
            "sub-crashy",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("sub-crashy", RestartPolicy::OnFailure)
        .unwrap();
    facade.start("sub-crashy").unwrap();

    // Wait for the reaper to detect the crash AND land the restart (the
    // `Restarted` cause in the committed log), tolerating the production 1s
    // backoff exactly like `tests/crash.rs`.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if committed_transitions(&facade, "sub-crashy")
            .iter()
            .any(|e| matches!(e.cause, TransitionCause::Restarted { .. }))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the crash was never restarted (no Restarted transition committed)"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    barrier(&facade);

    let (events, lagged) = test_support::drain_subscription(&mut sub);
    assert!(lagged.is_none());
    let received = transitions_of(&events);
    let committed = committed_transitions(&facade, "sub-crashy");
    assert_eq!(
        received
            .into_iter()
            .cloned()
            .collect::<Vec<TransitionEvent>>(),
        committed,
        "the crash/restart leg's bus stream equals the durable log"
    );

    // The crash and the restart BOTH ride the bus, in commit order.
    let crashed = events
        .iter()
        .position(|e| matches!(e, EngineEvent::Transition(t) if matches!(t.cause, TransitionCause::Crashed { .. })))
        .expect("the crashed transition was delivered");
    let restarted = events
        .iter()
        .position(|e| matches!(e, EngineEvent::Transition(t) if matches!(t.cause, TransitionCause::Restarted { .. })))
        .expect("the restarted transition was delivered");
    assert!(
        crashed < restarted,
        "the crash commits before the policy's restart"
    );

    // Tidy up: the restarted process lingers; stop it through the facade.
    wait_until_state(
        &facade,
        "sub-crashy",
        |s| s == hemaka_engine::LifecycleState::Running,
        Duration::from_secs(30),
        "the restarted instance to settle running",
    );
    let _ = facade.stop("sub-crashy", Some(Duration::from_secs(5)));
}

// ---------------------------------------------------------------------------
// Family (c): slow subscriber — no stall, Lagged, resync
// ---------------------------------------------------------------------------

#[test]
fn stalled_subscriber_never_stalls_supervision_and_observes_lagged() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    ManifestFixture::lingering("sub-slow").write(manifest.path());

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    let facade = engine.blocking();

    // The receiver attaches and then STALLS — nothing drains it until every
    // publish below is long done.
    let mut sub = facade.subscribe();

    facade
        .register_with_adapter(
            "sub-slow",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade.start("sub-slow").unwrap();
    wait_until_state(
        &facade,
        "sub-slow",
        |s| s == hemaka_engine::LifecycleState::Running,
        Duration::from_secs(30),
        "the instance to start",
    );

    // Publish well past the channel capacity with REAL supervision work: each
    // pause/resume pair commits (and publishes) two transitions. The cycle
    // count is DERIVED from the capacity const (capacity/2 pairs + margin), so
    // raising EVENT_BUS_CAPACITY cannot silently void this test's `total >
    // EVENT_BUS_CAPACITY` premise. The stalled receiver cannot back-pressure
    // any of this — the loop simply completes.
    let cycles = EVENT_BUS_CAPACITY / 2 + 40;
    let loop_deadline = Instant::now() + Duration::from_secs(120);
    for i in 0..cycles {
        assert!(
            Instant::now() < loop_deadline,
            "the pause/resume loop stalled before cycle {i}/{cycles} — a stuck \
             pause/resume would otherwise block the suite indefinitely"
        );
        facade
            .pause("sub-slow")
            .expect("pause (supervision unaffected)");
        facade
            .resume("sub-slow")
            .expect("resume (supervision unaffected)");
    }
    let status = facade.instance_status("sub-slow").unwrap();
    assert_eq!(
        status.instance.state,
        hemaka_engine::LifecycleState::Running,
        "the instance reached its expected state despite the stalled subscriber"
    );
    let committed = committed_transitions(&facade, "sub-slow");
    let total = committed.len() as u64;
    assert_eq!(
        total,
        2 + 2 * cycles as u64,
        "every transition committed while nobody drained"
    );
    assert!(
        total > EVENT_BUS_CAPACITY as u64,
        "the test must exceed the capacity"
    );

    // Now the stalled receiver catches up: first the Lagged marker naming the
    // dropped prefix, then the retained window — which corresponds EXACTLY to
    // the durable log's tail (nothing committed is ever mis-delivered).
    let (events, lagged) = test_support::drain_subscription(&mut sub);
    let lagged = lagged.expect("a stalled receiver past the capacity observes Lagged");
    assert_eq!(
        lagged,
        total - EVENT_BUS_CAPACITY as u64,
        "exactly the events past the window were dropped"
    );
    assert_eq!(
        events.len() as u64,
        EVENT_BUS_CAPACITY as u64,
        "the retained window is the full capacity"
    );
    for (i, event) in events.iter().enumerate() {
        match event {
            EngineEvent::Transition(t) => {
                assert_eq!(
                    t,
                    &committed[lagged as usize + i],
                    "retained event {i} is durable-log entry {} in order",
                    lagged as usize + i
                );
            }
            other => panic!("only transitions publish here, got {other:?}"),
        }
    }

    // Resync: the receiver keeps working — a NEW publish after the lag is
    // delivered (and equals the log's new tail).
    facade.pause("sub-slow").expect("the post-lag pause");
    let resynced = sub
        .recv()
        .expect("the resynced receiver receives the new event");
    let mut committed = committed;
    committed.push(
        committed_transitions(&facade, "sub-slow")
            .pop()
            .expect("new tail"),
    );
    match resynced {
        EngineEvent::Transition(t) => {
            assert_eq!(
                Some(&t),
                committed.last(),
                "the post-lag event is the new tail"
            );
            assert_eq!(t.new_state, hemaka_engine::LifecycleState::Paused);
        }
        other => panic!("expected the paused transition, got {other:?}"),
    }

    facade
        .resume("sub-slow")
        .expect("resume to leave the instance running");
    let _ = facade.stop("sub-slow", Some(Duration::from_secs(5)));
}

// ---------------------------------------------------------------------------
// Family (d): fan-out — two subscribers, independent full sequences
// ---------------------------------------------------------------------------

#[test]
fn two_subscribers_fan_out_independently() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    ManifestFixture::lingering("sub-fanout").write(manifest.path());

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    let facade = engine.blocking();

    // BOTH receivers attach before anything publishes.
    let mut sub_a = facade.subscribe();
    let mut sub_b = facade.subscribe();

    facade
        .register_with_adapter(
            "sub-fanout",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade.start("sub-fanout").unwrap();
    wait_until_state(
        &facade,
        "sub-fanout",
        |s| s == hemaka_engine::LifecycleState::Running,
        Duration::from_secs(30),
        "the instance to start",
    );
    facade.pause("sub-fanout").unwrap();
    facade.resume("sub-fanout").unwrap();
    facade
        .stop("sub-fanout", Some(Duration::from_secs(5)))
        .unwrap();
    barrier(&facade);

    let (events_a, lag_a) = test_support::drain_subscription(&mut sub_a);
    let (events_b, lag_b) = test_support::drain_subscription(&mut sub_b);
    assert!(lag_a.is_none() && lag_b.is_none());
    assert!(!events_a.is_empty(), "the sequence is non-empty");
    assert_eq!(
        events_a, events_b,
        "both subscribers independently received the full sequence"
    );
    // And it is the real flow: start edges, the pause round trip, the stop.
    let shapes: Vec<(String, String)> = events_a
        .iter()
        .map(|e| match e {
            EngineEvent::Transition(t) => {
                (format!("{}", t.prior_state), format!("{}", t.new_state))
            }
            _ => ("<other>".to_string(), "<other>".to_string()),
        })
        .collect();
    assert!(shapes.contains(&(String::from("registered"), String::from("starting"))));
    assert!(shapes.contains(&(String::from("running"), String::from("paused"))));
    assert!(shapes.contains(&(String::from("paused"), String::from("running"))));
    assert!(shapes.contains(&(String::from("stopping"), String::from("stopped"))));
}

// ---------------------------------------------------------------------------
// Family (e): per-instance FIFO with interleaved instances
// ---------------------------------------------------------------------------

#[test]
fn interleaved_instances_keep_per_instance_fifo() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    ManifestFixture::lingering("sub-fifo").write(manifest.path());

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    let facade = engine.blocking();
    let mut sub = facade.subscribe();

    // Two instances registered off the SAME manifest, driven so their events
    // interleave on the one bus.
    for name in ["fifo-a", "fifo-b"] {
        facade
            .register_with_adapter(name, &AdapterRef::Manifest(manifest.path().to_path_buf()))
            .unwrap();
    }
    facade.start("fifo-a").unwrap();
    facade.start("fifo-b").unwrap();
    for name in ["fifo-a", "fifo-b"] {
        wait_until_state(
            &facade,
            name,
            |s| s == hemaka_engine::LifecycleState::Running,
            Duration::from_secs(30),
            &format!("{name} to start"),
        );
    }

    // Deterministic interleave: every facade call is a synchronous
    // commit+publish, so the GLOBAL order below is exact.
    facade.pause("fifo-a").unwrap();
    facade.pause("fifo-b").unwrap();
    facade.resume("fifo-a").unwrap();
    facade.resume("fifo-b").unwrap();
    barrier(&facade);

    let (events, lagged) = test_support::drain_subscription(&mut sub);
    assert!(lagged.is_none());

    // The exact expected global sequence: A's start edges, B's start edges,
    // then the alternating pause/resume edges in call order.
    let expected: Vec<(
        &str,
        hemaka_engine::LifecycleState,
        hemaka_engine::LifecycleState,
    )> = vec![
        (
            "fifo-a",
            hemaka_engine::LifecycleState::Registered,
            hemaka_engine::LifecycleState::Starting,
        ),
        (
            "fifo-a",
            hemaka_engine::LifecycleState::Starting,
            hemaka_engine::LifecycleState::Running,
        ),
        (
            "fifo-b",
            hemaka_engine::LifecycleState::Registered,
            hemaka_engine::LifecycleState::Starting,
        ),
        (
            "fifo-b",
            hemaka_engine::LifecycleState::Starting,
            hemaka_engine::LifecycleState::Running,
        ),
        (
            "fifo-a",
            hemaka_engine::LifecycleState::Running,
            hemaka_engine::LifecycleState::Paused,
        ),
        (
            "fifo-b",
            hemaka_engine::LifecycleState::Running,
            hemaka_engine::LifecycleState::Paused,
        ),
        (
            "fifo-a",
            hemaka_engine::LifecycleState::Paused,
            hemaka_engine::LifecycleState::Running,
        ),
        (
            "fifo-b",
            hemaka_engine::LifecycleState::Paused,
            hemaka_engine::LifecycleState::Running,
        ),
    ];
    let mut got: Vec<(
        &str,
        hemaka_engine::LifecycleState,
        hemaka_engine::LifecycleState,
    )> = Vec::new();
    for event in &events {
        match event {
            EngineEvent::Transition(t) => {
                got.push((t.instance.as_str(), t.prior_state, t.new_state))
            }
            other => panic!("only transitions publish here, got {other:?}"),
        }
    }
    assert_eq!(
        got, expected,
        "the global bus order is the exact call order"
    );

    // Per-instance FIFO: each instance's subsequence equals its own durable
    // log in order (the interleaving never scrambles an instance's stream).
    for name in ["fifo-a", "fifo-b"] {
        let mine: Vec<TransitionEvent> = events
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Transition(t) if t.instance.as_str() == name => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            mine,
            committed_transitions(&facade, name),
            "{name}'s bus subsequence equals its durable log"
        );
    }

    // ---- BH15: the USAGE family under REAL interleaving. Two more instances
    // run the usage-emitting fixture CONCURRENTLY, so their events reach the
    // bus through overlapping reaper ingestion passes. No strict alternation
    // is asserted (the reaper drains each instance's captured batch per tick,
    // so the global order may batch by instance) — what MUST hold, and what
    // any cross-instance scrambling would break, is that each instance's
    // received usage subsequence equals its committed ledger rows EXACTLY, in
    // commit order.
    let usage_manifest = TempDir::new().unwrap();
    let usage_manifest_dir = uj3::write_flow_manifest(usage_manifest.path()); // --emit-usage 5
    for name in ["fifo-usage-a", "fifo-usage-b"] {
        facade
            .register_with_adapter(name, &AdapterRef::Manifest(usage_manifest_dir.clone()))
            .unwrap();
    }
    facade.start("fifo-usage-a").unwrap();
    facade.start("fifo-usage-b").unwrap();
    wait_for_usage_rows(
        state.path(),
        "fifo-usage-a",
        uj3::EMIT_EVENTS,
        Duration::from_secs(30),
    );
    wait_for_usage_rows(
        state.path(),
        "fifo-usage-b",
        uj3::EMIT_EVENTS,
        Duration::from_secs(30),
    );
    uj3::stop_all_resilient(&facade, &["fifo-usage-a", "fifo-usage-b"], uj3::STOP_WINDOW);
    barrier(&facade);

    let (events, lagged) = test_support::drain_subscription(&mut sub);
    assert!(lagged.is_none());
    for name in ["fifo-usage-a", "fifo-usage-b"] {
        let mine: Vec<EngineEvent> = events
            .iter()
            .filter_map(|e| match e {
                EngineEvent::UsageUpdate(u) if u.event.instance.as_str() == name => Some(e.clone()),
                _ => None,
            })
            .collect();
        let mine = usage_of(&mine);
        assert_eq!(
            mine.len() as u64,
            uj3::EMIT_EVENTS,
            "{name}: every committed usage event was delivered"
        );
        assert_usage_matches_rows(&mine, &committed_usage_rows(state.path(), name), name);
    }

    for name in ["fifo-a", "fifo-b"] {
        let _ = facade.stop(name, Some(Duration::from_secs(5)));
    }
}

// ---------------------------------------------------------------------------
// VG1: a failed durable append publishes NOTHING
// ---------------------------------------------------------------------------

#[test]
fn a_failed_transition_append_publishes_nothing_and_the_next_commit_delivers() {
    // "Never publishes what did not commit", mutation-killed: if the publish
    // were hoisted above the append gate (or fired on the error arm), the
    // obstructed pause WOULD reach the bus. The obstruction — the instance's
    // transition-log file replaced by a DIRECTORY — makes every append fail on
    // every OS (opening a directory for append is an error, no OS cfg here).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    ManifestFixture::lingering("sub-silent").write(manifest.path());

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    let facade = engine.blocking();
    let mut sub = facade.subscribe();

    facade
        .register_with_adapter(
            "sub-silent",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade.start("sub-silent").unwrap();
    wait_until_state(
        &facade,
        "sub-silent",
        |s| s == hemaka_engine::LifecycleState::Running,
        Duration::from_secs(30),
        "the instance to start",
    );
    barrier(&facade);
    let (baseline, lagged) = test_support::drain_subscription(&mut sub);
    assert!(lagged.is_none());
    assert_eq!(
        transitions_of(&baseline).len(),
        2,
        "the start edges delivered before the obstruction"
    );

    // Obstruct: replace the transition log with a directory (append → error).
    let log: PathBuf = state
        .path()
        .join("agents")
        .join("sub-silent")
        .join("logs")
        .join("instance.log");
    std::fs::remove_file(&log).expect("remove the transition log");
    std::fs::create_dir(&log).expect("replace it with a directory");

    // The transition fails at its durable append — and publishes nothing.
    assert!(
        facade.pause("sub-silent").is_err(),
        "the obstructed append must fail the transition"
    );
    let (quiet, lagged) = test_support::drain_subscription(&mut sub);
    assert!(lagged.is_none());
    assert!(
        quiet.is_empty(),
        "a failed commit must publish NOTHING, got {quiet:?}"
    );

    // Clear the obstruction: the next commit lands durably AND delivers.
    std::fs::remove_dir(&log).expect("clear the obstruction");
    facade.resume("sub-silent").expect("resume after clearing");
    barrier(&facade);
    let (after, lagged) = test_support::drain_subscription(&mut sub);
    assert!(lagged.is_none());
    let resumed = transitions_of(&after);
    assert_eq!(
        resumed.len(),
        1,
        "exactly the post-clear commit delivered: {after:?}"
    );
    assert_eq!(
        resumed[0].new_state,
        hemaka_engine::LifecycleState::Running,
        "the delivered event is the successful commit"
    );

    // The failed event is on NEITHER surface: no paused edge ever published
    // (baseline or after), and the durable log — recreated by the first
    // post-clear append — holds EXACTLY what was delivered since the
    // obstruction (the paused append never landed, so it is missing from both
    // surfaces equally; the obstruction itself destroyed the pre-existing log
    // lines, which is the file's loss, not the bus's).
    assert!(
        transitions_of(&baseline)
            .iter()
            .all(|t| t.new_state != hemaka_engine::LifecycleState::Paused),
        "no paused edge delivered before the obstruction"
    );
    assert!(
        resumed
            .iter()
            .all(|t| t.new_state != hemaka_engine::LifecycleState::Paused),
        "the failed append must never publish"
    );
    assert_eq!(
        committed_transitions(&facade, "sub-silent"),
        vec![resumed[0].clone()],
        "the post-clear durable log equals exactly what was delivered"
    );

    let _ = facade.stop("sub-silent", Some(Duration::from_secs(5)));
}

#[test]
fn a_failed_breach_append_still_enforces_but_publishes_no_breach() {
    // VG1 (the breach arm): with breaches.log obstructed, the breach record
    // fails to append (surfaced as the FR-21 diagnostic) while enforcement
    // STILL pauses. The bus must deliver the paused TRANSITION (which committed
    // to the healthy transition log) and never a BudgetBreach for the record
    // that failed — killing the "publish on the append error arm" mutation.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let manifest_dir = uj3::write_flow_manifest(manifest.path());

    // Obstruct BEFORE start: pre-create the logs dir with breaches.log as a
    // directory (the engine's own dir-create is idempotent; only the append
    // to the directory fails).
    let breaches_log: PathBuf = state
        .path()
        .join("agents")
        .join(uj3::FLOW_INSTANCE)
        .join("logs")
        .join("breaches.log");
    std::fs::create_dir_all(breaches_log.parent().unwrap()).expect("create the logs dir");
    std::fs::create_dir(&breaches_log).expect("obstruct the breach log");

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    let facade = engine.blocking();
    let mut sub = facade.subscribe();

    facade
        .register_with_adapter(uj3::FLOW_INSTANCE, &AdapterRef::Manifest(manifest_dir))
        .expect("register the flow's agent");
    for (key, value) in uj3::flow_config_pairs() {
        facade
            .set_config(uj3::FLOW_INSTANCE, key, value)
            .unwrap_or_else(|e| panic!("set_config {key}={value} failed: {e}"));
    }
    facade.start(uj3::FLOW_INSTANCE).expect("start");

    // Enforcement proceeds despite the failed breach record: the pause lands.
    uj3::wait_for_state(
        state.path(),
        uj3::FLOW_INSTANCE,
        hemaka_engine::LifecycleState::Paused,
        uj3::STATE_POLL_BUDGET,
    );
    barrier(&facade);
    let (events, lagged) = test_support::drain_subscription(&mut sub);
    assert!(lagged.is_none());

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, EngineEvent::BudgetBreach(_))),
        "NO breach event may publish for a failed breach append: {events:?}"
    );
    let committed = committed_transitions(&facade, uj3::FLOW_INSTANCE);
    let received: Vec<TransitionEvent> = transitions_of(&events).into_iter().cloned().collect();
    assert_eq!(
        received, committed,
        "the transition surface stays exactly mirrored"
    );
    assert!(
        committed
            .iter()
            .any(|e| e.new_state == hemaka_engine::LifecycleState::Paused),
        "enforcement still paused the instance"
    );
    // The durable breach record failed too — neither surface carries it.
    assert!(
        uj3::read_breach_events(state.path(), uj3::FLOW_INSTANCE).is_empty(),
        "the obstructed breach log must hold no record"
    );

    let _ = facade.stop(uj3::FLOW_INSTANCE, Some(uj3::STOP_WINDOW));
}

// ---------------------------------------------------------------------------
// VG2: a replayed usage batch publishes no duplicate
// ---------------------------------------------------------------------------

#[test]
fn a_replayed_batch_publishes_no_duplicate_usage_event() {
    // The metering.rs `--replay-usage` pattern, subscribed: the agent re-emits
    // sequence 0 after its batch; the ledger classifies it DuplicateReplay and
    // inserts NOTHING — so the bus must deliver exactly the committed rows,
    // with no second sequence-0 payload. Hoisting the usage publish out of the
    // Inserted/DuplicateReplay match would double-deliver here.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // The replay fixture: the SHARED replay-batch preset (story 10-1).
    ManifestFixture::replay_batch("sub-replay", 3).write(manifest.path());

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    let facade = engine.blocking();
    let mut sub = facade.subscribe();

    facade
        .register_with_adapter(
            "sub-replay",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade.start("sub-replay").unwrap();

    // Wait for the 3 distinct events to commit, then give the reaper the same
    // generous settle metering.rs uses for this exact fixture so the replayed
    // sequence-0 line is definitely drained AND classified (a duplicate → the
    // Inserted arm is never taken → nothing publishes).
    wait_for_usage_rows(state.path(), "sub-replay", 3, Duration::from_secs(30));
    std::thread::sleep(Duration::from_millis(800));
    barrier(&facade);

    let (events, lagged) = test_support::drain_subscription(&mut sub);
    assert!(lagged.is_none());
    let usage = usage_of(&events);
    let rows = committed_usage_rows(state.path(), "sub-replay");
    assert_eq!(
        rows.len(),
        3,
        "the replay adds no ledger row (the committed truth)"
    );
    assert_usage_matches_rows(&usage, &rows, "the replayed batch");
    let sequences: Vec<u64> = usage.iter().map(|u| u.event.sequence).collect();
    assert_eq!(
        sequences,
        vec![0, 1, 2],
        "no duplicate sequence 0 delivered"
    );

    let _ = facade.stop("sub-replay", Some(Duration::from_secs(5)));
}

// ---------------------------------------------------------------------------
// BH3: the async surface — a real tokio consumer over the raw receiver
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_async_consumer_receives_the_flow_over_the_raw_receiver() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    ManifestFixture::lingering("sub-async").write(manifest.path());
    let state_path = state.path().to_path_buf();

    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    // The RAW receiver (async consumers' surface): subscribing is sync and
    // lock-free, taken here before any flow traffic.
    let mut rx = engine.subscribe();

    // The facade blocks its caller, so the flow is driven from a plain thread
    // (never from inside this test's runtime) while the async consumer awaits.
    let driver = std::thread::spawn(move || {
        let facade = engine.blocking();
        facade
            .register_with_adapter(
                "sub-async",
                &AdapterRef::Manifest(manifest.path().to_path_buf()),
            )
            .unwrap();
        facade.start("sub-async").unwrap();
        wait_until_state(
            &facade,
            "sub-async",
            |s| s == hemaka_engine::LifecycleState::Running,
            Duration::from_secs(30),
            "the instance to start",
        );
        facade.pause("sub-async").unwrap();
        facade.resume("sub-async").unwrap();
        facade
            .stop("sub-async", Some(Duration::from_secs(5)))
            .unwrap();
    });

    // Every commit must arrive over `recv().await` within a bounded wait — a
    // lost publish surfaces as the overall deadline, never a hang. `Closed`
    // (the driver exiting drops the engine) ends the stream and is handled by
    // the completeness assertion against the committed log below.
    let overall = Instant::now() + Duration::from_secs(60);
    let mut received: Vec<EngineEvent> = Vec::new();
    loop {
        if driver.is_finished() {
            break;
        }
        assert!(
            Instant::now() < overall,
            "the async consumer never received the whole flow"
        );
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Ok(event)) => received.push(event),
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                panic!("a mini-flow never lags, got Lagged({n})")
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Err(_quiet_window) => {}
        }
    }
    driver.join().expect("the driver thread");
    // Everything published precedes the driver's exit; drain the tail. A
    // `Closed` here is the engine's sender side dropping with it — end of
    // stream, not an error.
    while let Ok(event) = rx.try_recv() {
        received.push(event);
    }

    assert!(!received.is_empty(), "the async consumer received the flow");
    for event in &received {
        assert_payload_validates(event);
    }
    let received: Vec<TransitionEvent> = transitions_of(&received).into_iter().cloned().collect();
    assert_eq!(
        received,
        uj3::read_transition_events(&state_path, "sub-async"),
        "the async receiver's stream equals the committed log (post-stop)"
    );
}
