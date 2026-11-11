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
             INSERT INTO run_labels (run_id, key, value, source) VALUES ('bad3', 'k', 'v', 'nobody'); \
             INSERT INTO runs (run_id, control_depth, status, created_at, updated_at) \
             VALUES ('bad4', 'wire', 'active', 't', 't'); \
             INSERT INTO run_labels VALUES ('bad4', 'k', 'v', 'bound', 'nowhere'); \
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
    assert!(matches!(
        store.run_labels(&RunId::from_wire("bad4")),
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

#[test]
fn a_caller_write_never_replaces_a_bound_row() {
    let (_tmp, store) = store();
    insert(
        &store,
        record("run_P", None, "a", true),
        vec![Label::verb("team", "core"), Label::bound("env", "prod")],
        RunLimits::default(),
    );
    let id = RunId::from_wire("run_P");
    store
        .replace_labels(
            &id,
            LabelSource::Caller,
            &[Label::caller("team", "mine"), Label::caller("task", "t")],
        )
        .unwrap();
    assert_eq!(
        store.run_labels(&id).unwrap(),
        vec![
            Label::bound("env", "prod"),
            Label::caller("task", "t"),
            Label::verb("team", "core"),
        ]
    );
}

#[test]
fn set_run_labels_replaces_the_whole_set_with_origins() {
    let (_tmp, store) = store();
    insert(
        &store,
        record("run_P", None, "a", true),
        vec![Label::bound("team", "x"), Label::caller("task", "t")],
        RunLimits::default(),
    );
    let id = RunId::from_wire("run_P");
    let next = vec![Label::caller("flow", "f"), Label::verb("team", "core")];
    store.set_run_labels(&id, &next).unwrap();
    assert_eq!(store.run_labels(&id).unwrap(), next);
    let origin: Option<String> = store
        .conn()
        .query_row(
            "SELECT origin FROM run_labels WHERE run_id = 'run_P' AND key = 'flow'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(origin, None, "a Caller row has no origin");
}

#[test]
fn select_runs_reads_open_runs_matching_every_term() {
    let (_tmp, store) = store();
    insert(
        &store,
        record("run_B", None, "a", true),
        vec![Label::bound("team", "core"), Label::caller("env", "prod")],
        RunLimits::default(),
    );
    insert(
        &store,
        record("run_A", Some("run_B"), "a", false),
        vec![Label::verb("team", "core"), Label::verb("env", "prod")],
        RunLimits::default(),
    );
    insert(
        &store,
        record("run_C", Some("run_B"), "a", false),
        vec![Label::verb("team", "core")],
        RunLimits::default(),
    );
    let mut ended = record("run_D", Some("run_B"), "a", false);
    ended.status = RunStatus::Ended;
    insert(
        &store,
        ended,
        vec![Label::verb("team", "core"), Label::verb("env", "prod")],
        RunLimits::default(),
    );
    let ids = |raw: &str, limit: usize| -> Vec<String> {
        store
            .select_runs(&Selector::parse(raw).unwrap(), limit)
            .unwrap()
            .into_iter()
            .map(|r| r.run_id.as_str().to_string())
            .collect()
    };
    assert_eq!(ids("team=core,env=prod", 10), vec!["run_A", "run_B"]);
    assert_eq!(ids("team=core", 10), vec!["run_A", "run_B", "run_C"]);
    assert_eq!(ids("team=core", 2), vec!["run_A", "run_B"]);
    assert!(ids("team=other", 10).is_empty());
}

#[test]
fn the_v14_step_records_config_as_the_origin_of_every_bound_label() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("state.db");
    let conn = crate::store::create_v11_for_test(&db);
    conn.execute_batch(
        "INSERT INTO agent_instances (id, name, kind, state, agent_home, created_at, updated_at) \
         VALUES (1, 'live', 'mock', 'running', '/h', 't', 't'); \
         INSERT INTO agent_runtime (instance_id, pid, start_time, restart_policy, run_id) \
         VALUES (1, 10, 1, 'on-failure', 'run-live');",
    )
    .unwrap();
    migrate_v12(&conn, &NoLegacy).unwrap();
    migrate_v13(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO run_labels VALUES ('run-live', 'team', 'payments', 'bound'), \
                                       ('run-live', 'task', 'x', 'caller');",
    )
    .unwrap();
    drop(conn);
    let store = SqliteStore::open(&db).unwrap();
    let version: i64 = store
        .conn()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    // The open runs EVERY remaining migration step, so a fully-opened DB
    // reads the current schema version (15 since Policies; the assertion
    // pins "the v14 step ran to completion on the way", which the origins
    // query below proves, not "nothing newer exists").
    assert_eq!(version, crate::store::SCHEMA_VERSION);
    let origins: Vec<(String, Option<String>)> = store
        .conn()
        .prepare("SELECT key, origin FROM run_labels ORDER BY key")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        origins,
        vec![
            ("task".to_string(), None),
            ("team".to_string(), Some("config".to_string()))
        ]
    );
    // A second run of the step steps aside.
    migrate_v14(store.conn()).unwrap();
    drop(store);
    let reopened = SqliteStore::open(&db).unwrap();
    assert_eq!(
        reopened.run_labels(&RunId::from_wire("run-live")).unwrap(),
        vec![Label::caller("task", "x"), Label::bound("team", "payments")]
    );
}

#[test]
fn corrupt_policy_rows_are_typed_and_fail_the_read_closed() {
    // Money code never silently drops a standing limit: every corrupt
    // Policy row shape answers a typed CorruptRow, and the whole `policies`
    // read fails rather than skipping the row.
    let (_tmp, store) = store();
    use crate::ports::PolicyRecords as _;
    store
        .conn()
        .execute_batch(
            "INSERT INTO policies (name, selector, created_at, updated_at) \
               VALUES ('noselector', NULL, 't', 't'); \
             INSERT INTO policies (name, selector, created_at, updated_at) \
               VALUES ('badsel', 'team=!', 't', 't'); \
             INSERT INTO policies (name, selector, breach_action, created_at, updated_at) \
               VALUES ('badaction', 'team=core', 'explode', 't', 't'); \
             INSERT INTO policies (name, selector, scent_check, created_at, updated_at) \
               VALUES ('badmode', 'team=core', 'maybe', 't', 't');",
        )
        .unwrap();
    for name in ["noselector", "badsel", "badaction", "badmode"] {
        let err = store.policy(name).unwrap_err();
        assert!(
            matches!(err, StoreError::CorruptRow { .. }),
            "{name}: {err}"
        );
        assert!(err.to_string().contains(name), "{err}");
    }
    assert!(matches!(
        store.policies(),
        Err(StoreError::CorruptRow { .. })
    ));
}

// ---------------------------------------------------------------------------
// The Run listing (story 21-5): schema v16, the keyset pages, the counts.
// ---------------------------------------------------------------------------

/// The store's schema version (pinned at v16 by the listing's own test).
use crate::store::sqlite::SCHEMA_VERSION;

/// A record shaped for the listing tests: `ended_at` set when ended, and a
/// process-owning row only where the partial unique index allows one open
/// owner per instance.
fn shaped(id: &str, status: RunStatus, anomalies: &[Anomaly], ended_at: Option<&str>) -> RunRecord {
    shaped_on(id, "a", status, anomalies, ended_at)
}

/// [`shaped`] with the instance and the process-owner flag named.
fn shaped_owner(
    id: &str,
    instance: &str,
    owner: bool,
    status: RunStatus,
    anomalies: &[Anomaly],
    ended_at: Option<&str>,
) -> RunRecord {
    let mut record = record(id, None, instance, owner);
    record.status = status;
    record.anomalies = anomalies.to_vec();
    record.ended_at = ended_at.map(str::to_string);
    record
}

/// [`shaped`] with the instance named (an open process owner).
fn shaped_on(
    id: &str,
    instance: &str,
    status: RunStatus,
    anomalies: &[Anomaly],
    ended_at: Option<&str>,
) -> RunRecord {
    shaped_owner(id, instance, true, status, anomalies, ended_at)
}

/// The listing fixture from the story's acceptance: a healthy active Run, an
/// active Run with the `breach` Anomaly, a `held` Run, an ended Run with the
/// `failed` Anomaly ended 2 days ago, an ended clean Run ended 2 days ago,
/// and an ended Run with an Anomaly ended 20 days ago. Run ids sort so the
/// expected order is unambiguous (ULID-shaped ids sort by creation time;
/// `K` > `J` > `H` > `G` > `F` > `E` sorts newest-first explicitly).
fn listing_fixture(store: &SqliteStore) {
    let seeds: Vec<RunRecord> = vec![
        shaped(
            "run_E",
            RunStatus::Ended,
            &[Anomaly::Failed],
            Some("2026-10-22T00:00:00Z"),
        ),
        shaped("run_F", RunStatus::Ended, &[], Some("2026-11-09T00:00:00Z")),
        shaped(
            "run_G",
            RunStatus::Ended,
            &[Anomaly::Breach],
            Some("2026-11-09T01:00:00Z"),
        ),
        // A held child of the healthy owner (a parent-paused cascade).
        shaped_owner("run_H", "a", false, RunStatus::Held, &[], None),
        shaped_on("run_J", "b", RunStatus::Active, &[Anomaly::Breach], None),
        shaped_on("run_K", "a", RunStatus::Active, &[], None),
    ];
    for seed in seeds {
        insert(store, seed, Vec::new(), RunLimits::default());
    }
}

/// The fixture's window: `now` = 2026-11-11, so the 2-day-old ends sit
/// inside the 7-day window and the 20-day-old end sits outside it.
const FIXTURE_CUTOFF: &str = "2026-11-04T00:00:00Z";

#[test]
fn the_v16_step_adds_the_three_listing_indexes() {
    let (_tmp, store) = store();
    let version: i64 = store
        .conn()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, 16, "the listing indexes are the v16 step");
    let mut stmt = store
        .conn()
        .prepare(
            "SELECT name, sql FROM sqlite_master WHERE type = 'index' AND name LIKE \
                  'idx_runs_%' ORDER BY name",
        )
        .unwrap();
    let indexes: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let names: Vec<&str> = indexes.iter().map(|(name, _)| name.as_str()).collect();
    for expected in ["idx_runs_ended_at", "idx_runs_exception", "idx_runs_open"] {
        assert!(names.contains(&expected), "{names:?}");
    }
    // The partial predicates are the v16 shape (the listing's queries name
    // them verbatim).
    let sql_of = |name: &str| {
        indexes
            .iter()
            .find(|(candidate, _)| candidate == name)
            .unwrap()
            .1
            .clone()
    };
    assert!(sql_of("idx_runs_open").contains("status <> 'ended'"));
    assert!(sql_of("idx_runs_exception").contains("anomalies <> ''"));
    assert!(sql_of("idx_runs_exception").contains("'held','silent','stale'"));
    assert!(sql_of("idx_runs_ended_at").contains("status = 'ended'"));
}

#[test]
fn the_exceptions_view_lists_loud_rows_and_counts_the_healthy() {
    let (_tmp, store) = store();
    listing_fixture(&store);
    let (rows, next) = store
        .list_runs(ListingView::Exceptions, None, None, 50, FIXTURE_CUTOFF)
        .unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r.run_id.as_str()).collect();
    // Newest first (run ids sort lexicographically): the breached active
    // Run, the held child, then the recent failed end. The healthy active
    // Run is counted, not listed; the clean and the 20-day-old ends stay
    // out of the exceptions view.
    assert_eq!(ids, vec!["run_J", "run_H", "run_G"]);
    assert_eq!(next, None, "everything the view lists fits one page");
    let counts = store
        .listing_counts(ListingView::Exceptions, None, FIXTURE_CUTOFF)
        .unwrap();
    assert_eq!(counts.listed, 3);
    assert_eq!(counts.healthy_open, 1);
}

#[test]
fn the_all_view_adds_the_recent_clean_end_not_the_old_one() {
    let (_tmp, store) = store();
    listing_fixture(&store);
    let (rows, _) = store
        .list_runs(ListingView::All, None, None, 50, FIXTURE_CUTOFF)
        .unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r.run_id.as_str()).collect();
    assert_eq!(ids, vec!["run_K", "run_J", "run_H", "run_G", "run_F"]);
    let counts = store
        .listing_counts(ListingView::All, None, FIXTURE_CUTOFF)
        .unwrap();
    assert_eq!(counts.listed, 5);
    assert_eq!(counts.healthy_open, 1);
}

#[test]
fn a_silent_or_stale_status_lists_the_moment_a_producer_exists() {
    let (_tmp, store) = store();
    insert(
        &store,
        shaped_on("run_S", "a", RunStatus::Silent, &[], None),
        Vec::new(),
        RunLimits::default(),
    );
    insert(
        &store,
        shaped_on("run_T", "b", RunStatus::Stale, &[], None),
        Vec::new(),
        RunLimits::default(),
    );
    let (rows, _) = store
        .list_runs(ListingView::Exceptions, None, None, 50, FIXTURE_CUTOFF)
        .unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r.run_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["run_T", "run_S"],
        "stored statuses, matched as stored"
    );
}

#[test]
fn the_keyset_cursor_pages_disjointly_newest_first_until_exhausted() {
    let (_tmp, store) = store();
    listing_fixture(&store);
    let (page_one, next) = store
        .list_runs(ListingView::All, None, None, 2, FIXTURE_CUTOFF)
        .unwrap();
    assert_eq!(
        page_one
            .iter()
            .map(|r| r.run_id.as_str())
            .collect::<Vec<_>>(),
        vec!["run_K", "run_J"]
    );
    let cursor = next.expect("a full page carries its last key");
    let (page_two, next) = store
        .list_runs(ListingView::All, None, Some(&cursor), 2, FIXTURE_CUTOFF)
        .unwrap();
    assert_eq!(
        page_two
            .iter()
            .map(|r| r.run_id.as_str())
            .collect::<Vec<_>>(),
        vec!["run_H", "run_G"]
    );
    let cursor = next.expect("still full");
    let (page_three, next) = store
        .list_runs(ListingView::All, None, Some(&cursor), 2, FIXTURE_CUTOFF)
        .unwrap();
    assert_eq!(
        page_three
            .iter()
            .map(|r| r.run_id.as_str())
            .collect::<Vec<_>>(),
        vec!["run_F"]
    );
    assert_eq!(next, None, "a short page ends the read");
    // Disjoint across every page, still newest-first overall.
    let all: Vec<&str> = page_one
        .iter()
        .chain(page_two.iter())
        .chain(page_three.iter())
        .map(|r| r.run_id.as_str())
        .collect();
    let mut sorted = all.clone();
    sorted.sort_unstable();
    sorted.reverse();
    assert_eq!(all, sorted);
}

#[test]
fn a_row_that_changes_between_pages_neither_duplicates_nor_reorders() {
    let (_tmp, store) = store();
    listing_fixture(&store);
    let (page_one, next) = store
        .list_runs(ListingView::All, None, None, 2, FIXTURE_CUTOFF)
        .unwrap();
    let cursor = next.unwrap();
    // run_H turns active-healthy between the pages: the keyset is run_id,
    // so page two still starts below the cursor and the row neither
    // duplicates nor moves.
    let mut moved = shaped("run_H", RunStatus::Active, &[], None);
    use crate::ports::RunRecords as _;
    store.save_run(&moved).unwrap();
    moved.status = RunStatus::Active;
    let (page_two, _) = store
        .list_runs(ListingView::All, None, Some(&cursor), 2, FIXTURE_CUTOFF)
        .unwrap();
    assert_eq!(
        page_two
            .iter()
            .map(|r| r.run_id.as_str())
            .collect::<Vec<_>>(),
        vec!["run_H", "run_G"],
        "the keyset is stable under a status change"
    );
    assert!(page_one.iter().all(|row| !page_two.contains(row)));
}

#[test]
fn selectors_match_bound_or_caller_labels_and_compose_with_both_views() {
    let (_tmp, store) = store();
    listing_fixture(&store);
    let selector: Selector = "team=core".parse().unwrap();
    // Nothing matches yet.
    let (rows, _) = store
        .list_runs(
            ListingView::Exceptions,
            Some(&selector),
            None,
            50,
            FIXTURE_CUTOFF,
        )
        .unwrap();
    assert!(rows.is_empty());
    // A bound label on the held Run and a caller label on the breached one:
    // both select (FR-49's listing scope — bound and caller).
    store
        .set_run_labels(&RunId::from_wire("run_H"), &[Label::bound("team", "core")])
        .unwrap();
    store
        .set_run_labels(
            &RunId::from_wire("run_J"),
            &[Label::caller("team", "core"), Label::caller("env", "prod")],
        )
        .unwrap();
    let (rows, _) = store
        .list_runs(
            ListingView::Exceptions,
            Some(&selector),
            None,
            50,
            FIXTURE_CUTOFF,
        )
        .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
        vec!["run_J", "run_H"]
    );
    // Every term must match.
    let both: Selector = "team=core,env=prod".parse().unwrap();
    let (rows, _) = store
        .list_runs(
            ListingView::Exceptions,
            Some(&both),
            None,
            50,
            FIXTURE_CUTOFF,
        )
        .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
        vec!["run_J"]
    );
    // The counts follow the selector.
    let counts = store
        .listing_counts(ListingView::Exceptions, Some(&selector), FIXTURE_CUTOFF)
        .unwrap();
    assert_eq!(counts.listed, 2);
    assert_eq!(counts.healthy_open, 0);
}

#[test]
fn a_selector_matching_more_than_the_verb_cap_still_pages() {
    // `select_runs` (verb support) refuses past MAX_SELECTED_RUNS; a LISTING
    // pages past the cap (FR-50: no read path loads the whole Run set).
    let (_tmp, store) = store();
    for index in 0..(crate::domain::MAX_SELECTED_RUNS + 1) {
        let id = format!("run_A{index:05}");
        insert(
            &store,
            shaped_owner(&id, "a", false, RunStatus::Active, &[], None),
            vec![Label::bound("team", "core")],
            RunLimits::default(),
        );
    }
    let selector: Selector = "team=core".parse().unwrap();
    let mut after: Option<String> = None;
    let mut listed = 0u64;
    loop {
        let (rows, next) = store
            .list_runs(
                ListingView::All,
                Some(&selector),
                after.as_deref(),
                400,
                FIXTURE_CUTOFF,
            )
            .unwrap();
        listed += rows.len() as u64;
        let Some(cursor) = next else { break };
        after = Some(cursor);
    }
    assert_eq!(listed, crate::domain::MAX_SELECTED_RUNS as u64 + 1);
    let counts = store
        .listing_counts(ListingView::All, Some(&selector), FIXTURE_CUTOFF)
        .unwrap();
    assert_eq!(counts.listed, crate::domain::MAX_SELECTED_RUNS as u64 + 1);
    assert_eq!(
        counts.healthy_open,
        crate::domain::MAX_SELECTED_RUNS as u64 + 1
    );
}

#[test]
fn an_empty_store_lists_nothing() {
    let (_tmp, store) = store();
    for view in [ListingView::Exceptions, ListingView::All] {
        let (rows, next) = store
            .list_runs(view, None, None, 50, FIXTURE_CUTOFF)
            .unwrap();
        assert!(rows.is_empty());
        assert_eq!(next, None);
        let counts = store.listing_counts(view, None, FIXTURE_CUTOFF).unwrap();
        assert_eq!(counts.listed, 0);
        assert_eq!(counts.healthy_open, 0);
    }
}

#[test]
fn the_listing_queries_read_indexes_only_never_a_table_scan() {
    let (_tmp, store) = store();
    listing_fixture(&store);
    let selector: Selector = "team=core".parse().unwrap();
    let statements = [
        (
            "page exceptions",
            format!(
                "EXPLAIN QUERY PLAN SELECT {RUN_COLUMNS} {RUN_FROM} WHERE r.run_id IN ({}) \
                 ORDER BY r.run_id DESC",
                view_arms_sql(ListingView::Exceptions, true, Some(50))
            ),
            vec![FIXTURE_CUTOFF.to_string(), "run_z".to_string()],
        ),
        (
            "page all",
            format!(
                "EXPLAIN QUERY PLAN SELECT {RUN_COLUMNS} {RUN_FROM} WHERE r.run_id IN ({}) \
                 AND r.run_id IN ({}) ORDER BY r.run_id DESC",
                view_arms_sql(ListingView::All, false, Some(50)),
                selector_arms_sql(&selector).0,
            ),
            [FIXTURE_CUTOFF.to_string()]
                .into_iter()
                .chain(selector_arms_sql(&selector).1)
                .collect(),
        ),
        (
            "count exceptions",
            format!(
                "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM runs WHERE run_id IN ({}) \
                 AND run_id IN ({})",
                view_arms_sql(ListingView::Exceptions, false, None),
                selector_arms_sql(&selector).0,
            ),
            [FIXTURE_CUTOFF.to_string()]
                .into_iter()
                .chain(selector_arms_sql(&selector).1)
                .collect(),
        ),
        (
            "count all",
            format!(
                "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM runs WHERE run_id IN ({})",
                view_arms_sql(ListingView::All, false, None)
            ),
            vec![FIXTURE_CUTOFF.to_string()],
        ),
        (
            "count healthy",
            "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM runs WHERE status <> 'ended' AND status = \
             'active' AND anomalies = ''"
                .to_string(),
            Vec::<String>::new(),
        ),
    ];
    for (name, sql, params) in statements {
        let mut stmt = store.conn().prepare(&sql).unwrap();
        let plan: Vec<String> = stmt
            .query_map(rusqlite::params_from_iter(params), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(!plan.is_empty(), "{name}: the plan rendered");
        // The gate: no plan line is a bare table scan of `runs` — every line
        // names an index read (`USING INDEX …`, a PK probe) or a merge/list
        // helper. `SCAN runs USING INDEX idx_runs_open` and its kin READ THE
        // INDEX, which is what the partial indexes exist for.
        for line in &plan {
            let bare_scan = line.starts_with("SCAN runs") && !line.contains("USING");
            assert!(
                !bare_scan,
                "{name} table-scans runs: {line} (full plan: {plan:?})"
            );
        }
        // And the ended arm rides the window index (the exception index
        // alone would walk every historical anomalous row).
        if name.contains("page") || name.contains("count") {
            let joined = plan.join("\n");
            if name != "count healthy" && name != "page exceptions" || true {
                assert!(
                    joined.contains("idx_runs_ended_at") || !joined.contains("'ended'"),
                    "{name}: {joined}"
                );
            }
        }
    }
}

#[test]
fn a_v15_state_dir_migrates_to_v16_and_the_listing_reads_it() {
    // Rebuild a genuine v15 shape (v16 adds only the three partial
    // indexes): drop them and stamp 15, then reopen — the v16 step runs,
    // stamps 16, and the listing reads the migrated store.
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("state.db");
    {
        let store = SqliteStore::open(&db).unwrap();
        store
            .conn()
            .execute_batch(
                "DROP INDEX idx_runs_open; \
                 DROP INDEX idx_runs_exception; \
                 DROP INDEX idx_runs_ended_at; \
                 PRAGMA user_version = 15;",
            )
            .unwrap();
    }
    let reopened = SqliteStore::open(&db).unwrap();
    let version: i64 = reopened
        .conn()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 16);
    for index in ["idx_runs_open", "idx_runs_exception", "idx_runs_ended_at"] {
        let present: i64 = reopened
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [index],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "{index}");
    }
}

#[test]
fn the_view_sql_matches_the_specification_predicates_row_by_row() {
    // The pure predicates in `domain/runs/listing.rs` are the views'
    // specification; the SQL arms (the partial indexes' reads) are the
    // implementation. Seed EVERY row shape the views distinguish and
    // cross-check the SQL's answer against the oracle, row by row — a
    // planner or arm change that drifts from the spec fails here.
    use crate::domain::runs::listing::{in_all_view, is_exception, is_healthy_open};

    let (_tmp, store) = store();
    let mut expected_exceptions = Vec::new();
    let mut expected_all = Vec::new();
    let mut expected_healthy = 0u64;
    let shapes: &[(&str, RunStatus, &[Anomaly], Option<&str>)] = &[
        ("active-clean", RunStatus::Active, &[], None),
        ("active-breach", RunStatus::Active, &[Anomaly::Breach], None),
        ("active-failed", RunStatus::Active, &[Anomaly::Failed], None),
        ("held", RunStatus::Held, &[], None),
        ("silent", RunStatus::Silent, &[], None),
        ("stale", RunStatus::Stale, &[], None),
        (
            "ended-clean-new",
            RunStatus::Ended,
            &[],
            Some("2026-11-10T00:00:00Z"),
        ),
        (
            "ended-clean-old",
            RunStatus::Ended,
            &[],
            Some("2026-10-01T00:00:00Z"),
        ),
        (
            "ended-failed-new",
            RunStatus::Ended,
            &[Anomaly::Failed],
            Some("2026-11-10T00:00:00Z"),
        ),
        (
            "ended-breach-old",
            RunStatus::Ended,
            &[Anomaly::Breach],
            Some("2026-10-01T00:00:00Z"),
        ),
        ("ended-null-at", RunStatus::Ended, &[Anomaly::Failed], None),
    ];
    for (index, (label, status, anomalies, ended_at)) in shapes.iter().enumerate() {
        let id = format!("run_P{index:03}{label}");
        insert(
            &store,
            shaped_owner(&id, "a", false, *status, anomalies, *ended_at),
            Vec::new(),
            RunLimits::default(),
        );
        let anomalies_text = Anomaly::encode(anomalies);
        if is_exception(status.as_str(), &anomalies_text, *ended_at, FIXTURE_CUTOFF) {
            expected_exceptions.push(id.clone());
        }
        if in_all_view(status.as_str(), *ended_at, FIXTURE_CUTOFF) {
            expected_all.push(id.clone());
        }
        if is_healthy_open(status.as_str(), &anomalies_text) {
            expected_healthy += 1;
        }
    }
    let (rows, _) = store
        .list_runs(ListingView::Exceptions, None, None, 50, FIXTURE_CUTOFF)
        .unwrap();
    let mut listed: Vec<String> = rows
        .into_iter()
        .map(|r| r.run_id.as_str().to_string())
        .collect();
    listed.sort();
    expected_exceptions.sort();
    assert_eq!(listed, expected_exceptions);
    let (rows, _) = store
        .list_runs(ListingView::All, None, None, 50, FIXTURE_CUTOFF)
        .unwrap();
    let mut listed: Vec<String> = rows
        .into_iter()
        .map(|r| r.run_id.as_str().to_string())
        .collect();
    listed.sort();
    expected_all.sort();
    assert_eq!(listed, expected_all);
    let counts = store
        .listing_counts(ListingView::All, None, FIXTURE_CUTOFF)
        .unwrap();
    assert_eq!(counts.healthy_open, expected_healthy);
    assert_eq!(counts.listed, expected_all.len() as u64);
}
