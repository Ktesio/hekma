//! Story 21-5: the Run listing's engine-level acceptance — the public
//! facade reads over a seeded state directory (rows written directly
//! through the perf seeding seam, no processes), covering the read the
//! CLI and the Control API project plus the SSE Selector filter's
//! subject-Run resolution over every event family.

use hekma_engine::{
    Anomaly, BreachAction, ControlDepth, Engine, EngineEvent, Label, LifecycleState, ListRunsQuery,
    ListingView, PerfRunRow, RunId, RunStatus, Selector, SelectorScope, TransitionCause,
    TransitionEvent,
};
use std::time::Duration;
use tempfile::TempDir;

/// A migrated store with one registered instance `demo`, via a live engine
/// (the instance row is seeded straight into the store, no process).
fn seeded_engine(base: &TempDir) -> Engine {
    {
        let engine = Engine::open(Some(base.path().to_path_buf())).unwrap();
        let _ = engine.blocking().policies();
    }
    {
        let conn = rusqlite::Connection::open(base.path().join("state.db")).unwrap();
        conn.execute(
            "INSERT INTO agent_instances (name, kind, state, agent_home, created_at, updated_at) \
             VALUES ('demo', 'mock', 'running', '/h', 't', 't')",
            [],
        )
        .unwrap();
    }
    Engine::open(Some(base.path().to_path_buf())).unwrap()
}

/// One bench row with the fields the test names.
fn row(
    run_id: &str,
    status: RunStatus,
    anomalies: &str,
    label: Option<(&str, &str)>,
) -> PerfRunRow {
    PerfRunRow {
        run_id: run_id.to_string(),
        status: status.as_str().to_string(),
        anomalies: anomalies.to_string(),
        ended_at: None,
        labels: label
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .into_iter()
            .collect(),
        resolution: None,
        subtree_tokens: if run_id.ends_with('U') { 300 } else { 0 },
    }
}

#[test]
fn the_seeding_seam_lists_runs_through_the_public_facade() {
    let base = TempDir::new().unwrap();
    let engine = seeded_engine(&base);
    let facade = engine.blocking();
    facade
        .seed_runs_for_perf(
            "demo",
            vec![
                row("run_benchK", RunStatus::Active, "", Some(("team", "core"))),
                row("run_benchH", RunStatus::Held, "", None),
                row(
                    "run_benchJ",
                    RunStatus::Active,
                    Anomaly::Breach.as_str(),
                    Some(("team", "core")),
                ),
                row("run_benchU", RunStatus::Active, "", Some(("env", "prod"))),
            ],
        )
        .unwrap();
    // The seam's refusal: an instance that does not exist.
    let err = facade.seed_runs_for_perf("ghost", vec![row("run_x", RunStatus::Active, "", None)]);
    assert!(err.is_err(), "an unknown instance refuses the seeding");

    let listing = facade.list_runs(ListRunsQuery::exceptions()).unwrap();
    let ids: Vec<&str> = listing
        .items
        .iter()
        .map(|item| item.record.run_id.as_str())
        .collect();
    assert_eq!(ids, vec!["run_benchJ", "run_benchH"], "newest first");
    assert_eq!(listing.counts.listed, 2);
    assert_eq!(listing.counts.healthy_open, 2);
    // The seeded rollup reads back through the row's usage (run_benchU's
    // 300 tokens) and a Selector composes over the seeded labels.
    let all = facade
        .list_runs(ListRunsQuery {
            view: ListingView::All,
            selector: Some("team=core".parse().unwrap()),
            after: None,
            limit: 0,
        })
        .unwrap();
    assert_eq!(all.items.len(), 2);
    let usage_row = facade
        .list_runs(ListRunsQuery {
            view: ListingView::All,
            selector: Some("env=prod".parse().unwrap()),
            after: None,
            limit: 0,
        })
        .unwrap();
    assert_eq!(usage_row.items[0].usage.input_tokens, 300);

    // A read-only engine reads the same rows (the `hekma ps` posture).
    let reader = Engine::open_read_only(Some(base.path().to_path_buf())).unwrap();
    let listing = reader
        .blocking()
        .list_runs(ListRunsQuery::exceptions())
        .unwrap();
    assert_eq!(listing.items.len(), 2);
}

#[test]
fn the_subject_run_resolution_covers_every_event_family() {
    let base = TempDir::new().unwrap();
    let engine = seeded_engine(&base);
    engine
        .blocking()
        .seed_runs_for_perf(
            "demo",
            vec![
                row("run_subJ", RunStatus::Active, "", Some(("team", "core"))),
                row("run_subE", RunStatus::Ended, "", None),
            ],
        )
        .unwrap();
    // The seeding seam publishes no snapshot, so every subject here
    // resolves through the one read-only store read (the open-Run fast
    // path is pinned in the engine's unit tests); the ended row keeps
    // resolving after its end (until compaction).
    let facade = engine.blocking();

    let run_event = EngineEvent::Run(hekma_engine::RunEvent {
        schema_version: hekma_engine::RUN_EVENT_SCHEMA_VERSION,
        change: hekma_engine::RunEventKind::LabelsChanged,
        run_id: RunId::from_wire("run_subJ"),
        parent_run_id: None,
        instance: Some("demo".to_string()),
        control_depth: ControlDepth::Announced,
        wire_path: false,
        status: RunStatus::Active,
        prior_status: None,
        prior_control_depth: None,
        prior_wire_path: None,
        anomalies: Vec::new(),
        labels: vec![Label::bound("team", "core")],
        prior_labels: None,
        cause: None,
        policies: Vec::new(),
        at: "t".to_string(),
    });
    let labels = facade
        .event_subject_labels(&run_event)
        .unwrap()
        .expect("the Run's labels");
    assert!(labels.contains(&Label::bound("team", "core")));
    // The Selector itself matches against them (the host filter's check).
    let selector: Selector = "team=core".parse().unwrap();
    assert!(selector.matches(&labels, SelectorScope::Any));

    // A transition belongs to the instance's open process-owning Run: none
    // exists (the seeded rows are children), so the filter drops it.
    let transition = EngineEvent::Transition(TransitionEvent::new(
        "demo",
        LifecycleState::Starting,
        LifecycleState::Running,
        TransitionCause::AdapterReady,
        "t",
    ));
    assert!(
        facade.event_subject_labels(&transition).unwrap().is_none(),
        "no open owner → dropped under a selector"
    );

    // Events naming no Run: delivered only without a Selector.
    for no_run in [
        EngineEvent::Policy(Box::new(hekma_engine::PolicyEvent {
            schema_version: hekma_engine::POLICY_EVENT_SCHEMA_VERSION,
            kind: hekma_engine::PolicyEventKind::Created,
            name: "p".to_string(),
            selector: None,
            limits: None,
            prior_limits: None,
            rates: Default::default(),
            matched: 0,
            fallbacks: 0,
            at: "t".to_string(),
        })),
        EngineEvent::BreachDefaultUpgraded(hekma_engine::BreachDefaultUpgradedEvent {
            schema_version: hekma_engine::POLICY_EVENT_SCHEMA_VERSION,
            instances: Vec::new(),
            at: "t".to_string(),
        }),
    ] {
        assert!(facade.event_subject_labels(&no_run).unwrap().is_none());
    }

    // A usage / breach event resolves by its run id, ended or not.
    let usage = EngineEvent::UsageUpdate(hekma_engine::UsageUpdateEvent::new(
        hekma_engine::UsageEvent {
            instance: "demo".to_string(),
            run_id: RunId::from_wire("run_subE"),
            input_tokens: 1,
            output_tokens: 1,
            cached_tokens: None,
            metering_source: "self-reported".to_string(),
            sequence: 0,
            occurred_at: "t".to_string(),
        },
    ));
    // (The ended row carries no label: the store read answers its absence
    // honestly, never a fabricated match.)
    assert!(facade
        .event_subject_labels(&usage)
        .unwrap()
        .is_some_and(|labels| labels.is_empty()));
    let breach = EngineEvent::BudgetBreach(hekma_engine::BudgetBreachEvent::new(
        "demo",
        "run_subJ",
        hekma_engine::BreachScope::PerRun,
        1,
        1,
        BreachAction::Warn,
        "self-reported",
        "t",
    ));
    assert!(facade
        .event_subject_labels(&breach)
        .unwrap()
        .is_some_and(|labels| labels.contains(&Label::bound("team", "core"))));

    // A Run that no longer exists: Ok(None), never an error.
    let ghost = EngineEvent::Run(hekma_engine::RunEvent {
        schema_version: hekma_engine::RUN_EVENT_SCHEMA_VERSION,
        change: hekma_engine::RunEventKind::Created,
        run_id: RunId::from_wire("run_ghost"),
        parent_run_id: None,
        instance: None,
        control_depth: ControlDepth::Observed,
        wire_path: false,
        status: RunStatus::Active,
        prior_status: None,
        prior_control_depth: None,
        prior_wire_path: None,
        anomalies: Vec::new(),
        labels: Vec::new(),
        prior_labels: None,
        cause: None,
        policies: Vec::new(),
        at: "t".to_string(),
    });
    assert!(facade.event_subject_labels(&ghost).unwrap().is_none());
}

#[test]
fn the_sequenced_bus_carries_the_outbox_seq_of_committed_events() {
    // One engine, one committed event (the policy create), one sequenced
    // receiver: the frame's seq IS the export's seq for the same event.
    let base = TempDir::new().unwrap();
    let engine = seeded_engine(&base);
    let mut sequenced = engine.subscribe_sequenced();
    let mut plain = engine.subscribe();
    engine
        .blocking()
        .create_policy(
            hekma_engine::PolicyChange {
                name: "sight".to_string(),
                selector: Some("team=core".parse().unwrap()),
                set: Default::default(),
                clear: Vec::new(),
                rates: Default::default(),
            },
            false,
        )
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let (seq, event) = loop {
        assert!(std::time::Instant::now() < deadline, "no sequenced frame");
        match sequenced.try_recv() {
            Ok(pair) => break pair,
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    assert!(matches!(event, EngineEvent::Policy(_)));
    let batch = engine.blocking().export_events(None, 0).unwrap();
    assert!(
        batch.events.iter().any(|entry| entry.seq == seq),
        "the bus seq {seq} is an outbox seq: {:?}",
        batch.events.iter().map(|e| e.seq).collect::<Vec<_>>()
    );
    // The plain ring is unchanged: the same event arrives bare, in the same
    // order.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let bare = loop {
        assert!(std::time::Instant::now() < deadline, "no plain frame");
        match plain.try_recv() {
            Ok(event) => break event,
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    assert_eq!(bare, event, "the plain ring carries the same event");
}

#[test]
fn a_transition_after_its_owner_run_ended_still_resolves_its_subject() {
    // A `stopped` transition publishes after the owner Run's end commits,
    // so the snapshot no longer holds the owner: the filter resolves the
    // instance's newest owner Run, ended or not, and a Selector still sees
    // the transition an operator most wants.
    let base = TempDir::new().unwrap();
    drop(seeded_engine(&base));
    {
        let conn = rusqlite::Connection::open(base.path().join("state.db")).unwrap();
        conn.execute_batch(
            "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
             instance_id, process_owner, created_at, updated_at, ended_at) SELECT \
             'run_OLD', 'supervised', 0, 'ended', '', id, 1, 't', 't', 't' \
             FROM agent_instances WHERE name = 'demo'; \
             INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, \
             instance_id, process_owner, created_at, updated_at, ended_at) SELECT \
             'run_OWN', 'supervised', 0, 'ended', '', id, 1, 't', 't', 't' \
             FROM agent_instances WHERE name = 'demo'; \
             INSERT INTO run_labels (run_id, key, value, source, origin) VALUES \
             ('run_OWN', 'team', 'core', 'bound', 'config');",
        )
        .unwrap();
    }
    let engine = Engine::open(Some(base.path().to_path_buf())).unwrap();
    let stopped = EngineEvent::Transition(TransitionEvent::new(
        "demo",
        LifecycleState::Stopping,
        LifecycleState::Stopped,
        TransitionCause::Command {
            command: "stop".to_string(),
        },
        "t",
    ));
    let labels = engine
        .blocking()
        .event_subject_labels(&stopped)
        .unwrap()
        .expect("the newest owner Run, though it has ended");
    assert_eq!(
        labels,
        vec![Label::bound("team", "core")],
        "run_OWN, not run_OLD"
    );
}
