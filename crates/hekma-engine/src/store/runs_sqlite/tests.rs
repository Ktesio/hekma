use super::*;
use crate::domain::{RunLimits, RunStatus};
use crate::ports::RunStore;
use crate::store::run_store_sqlite::{migrate_v12, NoLegacy};
use tempfile::TempDir;

fn name(raw: &str) -> InstanceName {
    InstanceName::new(raw).unwrap()
}

/// A migrated store with instances `a` (and `b`) registered.
fn store() -> (TempDir, SqliteStore) {
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(&tmp.path().join("state.db")).unwrap();
    for instance in ["a", "b"] {
        store
            .conn()
            .execute(
                "INSERT INTO agent_instances (name, kind, state, agent_home, created_at, \
                 updated_at) VALUES (?1, 'mock', 'running', '/h', 't', 't')",
                [instance],
            )
            .unwrap();
    }
    (tmp, store)
}

fn record(id: &str, parent: Option<&str>, instance: &str, owner: bool) -> RunRecord {
    RunRecord {
        run_id: RunId::from_wire(id),
        control_depth: if owner {
            ControlDepth::Supervised
        } else {
            ControlDepth::Announced
        },
        wire_path: true,
        status: RunStatus::Active,
        anomalies: Vec::new(),
        parent_run_id: parent.map(RunId::from_wire),
        instance: Some(instance.to_string()),
        process_owner: owner,
        end_cause: None,
        created_at: "2026-11-02T00:00:00Z".to_string(),
        updated_at: "2026-11-02T00:00:00Z".to_string(),
        ended_at: None,
    }
}

fn insert(store: &SqliteStore, record: RunRecord, labels: Vec<Label>, limits: RunLimits) {
    store
        .insert_run(&NewRun {
            record,
            labels,
            limits,
            identity: None,
        })
        .unwrap();
}

#[test]
fn a_run_round_trips_with_its_labels_limits_and_identity() {
    let (_tmp, store) = store();
    insert(
        &store,
        record("run_P", None, "a", true),
        vec![Label::bound("team", "x")],
        RunLimits::default(),
    );
    store
        .insert_run(&NewRun {
            record: record("run_C", Some("run_P"), "a", false),
            labels: vec![Label::caller("task", "t1")],
            limits: RunLimits {
                token_per_run: Some(100),
                dollar_per_run_micros: Some(Micros(5)),
                breach_action: Some(BreachAction::Warn),
            },
            identity: Some(("host".to_string(), "job-1".to_string())),
        })
        .unwrap();
    let child = store
        .run_record(&RunId::from_wire("run_C"))
        .unwrap()
        .unwrap();
    assert_eq!(child.parent_run_id, Some(RunId::from_wire("run_P")));
    assert_eq!(child.instance.as_deref(), Some("a"));
    assert!(!child.process_owner);
    assert_eq!(
        store.run_labels(&RunId::from_wire("run_C")).unwrap(),
        vec![Label::caller("task", "t1")]
    );
    let limits = store.run_limits(&RunId::from_wire("run_C")).unwrap();
    assert_eq!(limits.token_per_run, Some(100));
    assert_eq!(limits.dollar_per_run_micros, Some(Micros(5)));
    assert_eq!(limits.action(), BreachAction::Warn);
    assert_eq!(
        store.identity_run("host", "job-1").unwrap(),
        Some(RunId::from_wire("run_C"))
    );
    assert_eq!(store.identity_run("host", "nope").unwrap(), None);
    assert_eq!(store.open_children(&RunId::from_wire("run_P")).unwrap(), 1);
    let chain: Vec<String> = store
        .run_chain(&RunId::from_wire("run_C"))
        .unwrap()
        .into_iter()
        .map(|r| r.run_id.as_str().to_string())
        .collect();
    assert_eq!(chain, vec!["run_C", "run_P"]);
    assert_eq!(
        store
            .open_process_run(&name("a"))
            .unwrap()
            .unwrap()
            .run_id
            .as_str(),
        "run_P"
    );
    assert!(store.open_process_run(&name("b")).unwrap().is_none());
    assert_eq!(store.open_runs().unwrap().len(), 2);
    assert!(store
        .run_record(&RunId::from_wire("missing"))
        .unwrap()
        .is_none());
}

#[test]
fn one_open_process_owning_run_per_instance() {
    let (_tmp, store) = store();
    insert(
        &store,
        record("run_1", None, "a", true),
        vec![],
        RunLimits::default(),
    );
    let second = store.insert_run(&NewRun {
        record: record("run_2", None, "a", true),
        labels: vec![],
        limits: RunLimits::default(),
        identity: None,
    });
    assert!(
        second.is_err(),
        "the partial unique index refuses a second owner"
    );
    // Once the first ends, a new one opens.
    let mut first = store
        .run_record(&RunId::from_wire("run_1"))
        .unwrap()
        .unwrap();
    first.status = RunStatus::Ended;
    store.save_run(&first).unwrap();
    insert(
        &store,
        record("run_2", None, "a", true),
        vec![],
        RunLimits::default(),
    );
}

#[test]
fn descendants_in_the_process_skip_ended_and_other_instances() {
    let (_tmp, store) = store();
    insert(
        &store,
        record("run_P", None, "a", true),
        vec![],
        RunLimits::default(),
    );
    insert(
        &store,
        record("run_C1", Some("run_P"), "a", false),
        vec![],
        RunLimits::default(),
    );
    insert(
        &store,
        record("run_G1", Some("run_C1"), "a", false),
        vec![],
        RunLimits::default(),
    );
    insert(
        &store,
        record("run_C2", Some("run_P"), "a", false),
        vec![],
        RunLimits::default(),
    );
    insert(
        &store,
        record("run_X", Some("run_P"), "b", false),
        vec![],
        RunLimits::default(),
    );
    let mut ended = store
        .run_record(&RunId::from_wire("run_C2"))
        .unwrap()
        .unwrap();
    ended.status = RunStatus::Ended;
    store.save_run(&ended).unwrap();
    let ids: Vec<String> = store
        .open_in_process_descendants(&RunId::from_wire("run_P"))
        .unwrap()
        .into_iter()
        .map(|r| r.run_id.as_str().to_string())
        .collect();
    assert_eq!(ids, vec!["run_C1", "run_G1"]);
}

#[test]
fn replacing_bound_labels_keeps_caller_labels_and_a_bound_key_wins() {
    let (_tmp, store) = store();
    insert(
        &store,
        record("run_P", None, "a", true),
        vec![Label::caller("task", "t"), Label::caller("team", "mine")],
        RunLimits::default(),
    );
    let id = RunId::from_wire("run_P");
    store
        .replace_labels(&id, LabelSource::Bound, &[Label::bound("team", "payments")])
        .unwrap();
    assert_eq!(
        store.run_labels(&id).unwrap(),
        vec![Label::caller("task", "t"), Label::bound("team", "payments")]
    );
}

#[test]
fn saving_an_unknown_run_is_not_found_and_corrupt_rows_are_typed() {
    let (_tmp, store) = store();
    let ghost = record("ghost", None, "a", true);
    assert!(matches!(
        store.save_run(&ghost),
        Err(StoreError::NotFound { .. })
    ));
    store
        .conn()
        .execute_batch(
            "INSERT INTO runs (run_id, control_depth, status, created_at, updated_at) \
             VALUES ('bad', 'sideways', 'active', 't', 't'); \
             INSERT INTO runs (run_id, control_depth, status, created_at, updated_at) \
             VALUES ('bad2', 'wire', 'sideways', 't', 't'); \
             INSERT INTO runs (run_id, control_depth, status, created_at, updated_at) \
             VALUES ('bad3', 'wire', 'active', 't', 't'); \
             INSERT INTO run_labels VALUES ('bad3', 'k', 'v', 'nobody'); \
             INSERT INTO run_limits VALUES ('bad3', 'tokens', 'per-run', 1, 'explode', 'host', 't');",
        )
        .unwrap();
    for id in ["bad", "bad2"] {
        assert!(matches!(
            store.run_record(&RunId::from_wire(id)),
            Err(StoreError::CorruptRow { .. })
        ));
    }
    let bad3 = RunId::from_wire("bad3");
    assert!(matches!(
        store.run_labels(&bad3),
        Err(StoreError::CorruptRow { .. })
    ));
    assert!(matches!(
        store.run_limits(&bad3),
        Err(StoreError::CorruptRow { .. })
    ));
    store
        .conn()
        .execute_batch(
            "DELETE FROM run_limits; \
             INSERT INTO run_limits VALUES ('bad3', 'apples', 'per-run', 1, 'warn', 'host', 't');",
        )
        .unwrap();
    assert!(matches!(
        store.run_limits(&bad3),
        Err(StoreError::CorruptRow { .. })
    ));
    // An unknown instance on a new Run is NotFound.
    assert!(matches!(
        store.insert_run(&NewRun {
            record: record("orphan", None, "zzz", true),
            labels: vec![],
            limits: RunLimits::default(),
            identity: None,
        }),
        Err(StoreError::NotFound { .. })
    ));
}

#[test]
fn usage_folds_into_every_ancestor_and_the_instance_once() {
    let (_tmp, store) = store();
    insert(
        &store,
        record("run_P", None, "a", true),
        vec![],
        RunLimits::default(),
    );
    insert(
        &store,
        record("run_C", Some("run_P"), "a", false),
        vec![],
        RunLimits::default(),
    );
    insert(
        &store,
        record("run_G", Some("run_C"), "a", false),
        vec![],
        RunLimits::default(),
    );
    let usage = |run: &str, sequence: u64, tokens: u64| crate::domain::UsageEvent {
        instance: "a".to_string(),
        run_id: RunId::from_wire(run),
        input_tokens: tokens,
        output_tokens: 0,
        cached_tokens: Some(0),
        metering_source: "self-reported".to_string(),
        sequence,
        occurred_at: "t".to_string(),
    };
    store.insert_usage(&usage("run_G", 1, 10), None, 1).unwrap();
    store.insert_usage(&usage("run_C", 1, 20), None, 1).unwrap();
    store.insert_usage(&usage("run_P", 1, 40), None, 1).unwrap();
    let total = |run: &str| {
        store
            .run_rollup(&name("a"), &RunId::from_wire(run))
            .unwrap()
            .totals
            .total_tokens()
    };
    assert_eq!(total("run_G"), 10);
    assert_eq!(total("run_C"), 30);
    assert_eq!(total("run_P"), 70);
    assert_eq!(
        store
            .instance_rollup(&name("a"))
            .unwrap()
            .totals
            .total_tokens(),
        70
    );
}

#[test]
fn an_unregistered_run_id_gets_an_ended_placeholder_once() {
    let (_tmp, store) = store();
    let id = RunId::from_wire("run-legacy");
    store.ensure_run_row(&id, &name("a")).unwrap();
    store.ensure_run_row(&id, &name("a")).unwrap();
    let row = store.run_record(&id).unwrap().unwrap();
    assert_eq!(row.status, RunStatus::Ended);
    assert_eq!(row.end_cause.as_deref(), Some("unregistered"));
    assert_eq!(row.instance.as_deref(), Some("a"));
}

#[test]
fn the_v13_step_backfills_live_and_historical_runs_and_references_latches() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("state.db");
    let conn = crate::store::create_v11_for_test(&db);
    conn.execute_batch(
        "INSERT INTO agent_instances (id, name, kind, state, agent_home, created_at, updated_at) \
         VALUES (1, 'live', 'mock', 'running', '/h', 't', 't'), \
                (2, 'held', 'mock', 'paused', '/h', 't', 't'), \
                (3, 'down', 'mock', 'failed', '/h', 't', 't'); \
         INSERT INTO agent_runtime (instance_id, pid, start_time, restart_policy, run_id) \
         VALUES (1, 10, 1, 'on-failure', 'run-live'), \
                (2, 11, 1, 'on-failure', 'run-held'), \
                (3, 12, 1, 'on-failure', 'run-down'); \
         INSERT INTO usage_events (instance_id, run_id, input_tokens, output_tokens, \
             metering_source, occurred_at, sequence) \
         VALUES (1, 'run-old', 5, 5, 'engine-observed', '2026-01-01T00:00:00Z', 1), \
                (1, 'run-live', 7, 0, 'engine-observed', '2026-02-01T00:00:00Z', 1);",
    )
    .unwrap();
    migrate_v12(&conn, &NoLegacy).unwrap();
    // A latch for a run id nothing else names (an older engine's).
    conn.execute_batch(
        "INSERT INTO run_latches VALUES ('run-ghost', 'tokens', 'per-run', 'pause', 't'); \
         INSERT INTO breach_events (instance_id, run_id, dimension, scope, action, event) \
         VALUES (2, 'run-breached', 'tokens', 'per-run', 'warn', '{}');",
    )
    .unwrap();
    migrate_v13(&conn).unwrap();
    // A second run steps aside.
    migrate_v13(&conn).unwrap();
    drop(conn);
    let store = SqliteStore::open(&db).unwrap();
    let get = |id: &str| store.run_record(&RunId::from_wire(id)).unwrap().unwrap();
    let live = get("run-live");
    assert_eq!(live.status, RunStatus::Active);
    assert!(live.process_owner);
    assert!(live.wire_path, "the last usage row was engine-observed");
    assert_eq!(live.created_at, "2026-02-01T00:00:00Z");
    assert_eq!(get("run-held").status, RunStatus::Held);
    let down = get("run-down");
    assert_eq!(
        down.status,
        RunStatus::Ended,
        "a failed instance's Run ends; a relaunch reopens it"
    );
    assert_eq!(down.anomalies, vec![Anomaly::Failed]);
    for history in ["run-old", "run-breached", "run-ghost"] {
        let row = get(history);
        assert_eq!(row.status, RunStatus::Ended, "{history}");
        assert_eq!(row.end_cause.as_deref(), Some("before-runs"));
    }
    assert_eq!(get("run-old").instance.as_deref(), Some("live"));
    // The latch table now references runs: an unknown run id is refused.
    let refused = store.conn().execute(
        "INSERT INTO run_latches VALUES ('nobody', 'tokens', 'per-run', 'pause', 't')",
        [],
    );
    assert!(refused.is_err());
    assert_eq!(
        store.latches(&RunId::from_wire("run-ghost")).unwrap().len(),
        1
    );
}
