use std::sync::Arc;

use super::ops::{self, RunFate, SupervisedRun};
use super::snapshot::SnapshotPublisher;
use super::*;
use crate::domain::name::InstanceName;
use crate::ports::RunRecords;
use crate::store::{SqliteStore, StoreWriter};
use tempfile::TempDir;

const AT: &str = "2026-11-02T00:00:00Z";

fn name(raw: &str) -> InstanceName {
    InstanceName::new(raw).unwrap()
}

fn store() -> (TempDir, SqliteStore) {
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(&tmp.path().join("state.db")).unwrap();
    store.execute_batch_for_test(
        "INSERT INTO agent_instances (name, kind, state, agent_home, created_at, updated_at) \
         VALUES ('a', 'mock', 'running', '/h', 't', 't')",
    );
    (tmp, store)
}

fn supervised(run_id: &RunId, wire_path: bool, fresh: bool) -> SupervisedRun {
    SupervisedRun {
        instance: name("a"),
        run_id: run_id.clone(),
        wire_path,
        labels: vec![Label::bound("team", "payments")],
        fresh,
    }
}

fn child(store: &SqliteStore, parent: RunParent) -> Result<RunId, RunError> {
    ops::open_child(store, &ChildRunRequest::under(parent), RunId::mint(), AT).map(|(id, _)| id)
}

fn status(store: &SqliteStore, id: &RunId) -> (RunStatus, Vec<Anomaly>, Option<String>) {
    let row = store.run_record(id).unwrap().unwrap();
    (row.status, row.anomalies, row.end_cause)
}

#[test]
fn a_fresh_start_creates_and_a_relaunch_keeps_the_supervised_run() {
    let (_tmp, store) = store();
    let first = RunId::mint();
    let created = ops::ensure_supervised(&store, &supervised(&first, true, true), AT).unwrap();
    assert_eq!(created.events.len(), 1);
    assert_eq!(created.events[0].change, RunEventKind::Created);
    assert_eq!(
        created.events[0].labels,
        vec![Label::bound("team", "payments")]
    );
    assert_eq!(created.entries.len(), 1);
    // A relaunch with the same id and facts changes nothing.
    let kept = ops::ensure_supervised(&store, &supervised(&first, true, false), AT).unwrap();
    assert!(kept.events.is_empty());
    // A wire-path change keeps the id and emits depth-changed.
    let moved = ops::ensure_supervised(&store, &supervised(&first, false, false), AT).unwrap();
    assert_eq!(moved.events.len(), 1);
    let event = &moved.events[0];
    assert_eq!(event.change, RunEventKind::DepthChanged);
    assert_eq!(event.run_id, first);
    assert_eq!(event.prior_wire_path, Some(true));
    assert!(!event.wire_path);
    // A fresh start ends the still-open Run as `replaced`.
    let second = RunId::mint();
    let replaced = ops::ensure_supervised(&store, &supervised(&second, true, true), AT).unwrap();
    let kinds: Vec<RunEventKind> = replaced.events.iter().map(|e| e.change).collect();
    assert_eq!(kinds, vec![RunEventKind::Ended, RunEventKind::Created]);
    assert_eq!(status(&store, &first).2.as_deref(), Some("replaced"));
    // A kept Run that already ended is reopened when its process runs again.
    // (The instance's other open Run ends as `replaced` first.)
    let reopened = ops::ensure_supervised(&store, &supervised(&first, false, false), AT).unwrap();
    let kinds: Vec<RunEventKind> = reopened.events.iter().map(|e| e.change).collect();
    assert_eq!(
        kinds,
        vec![RunEventKind::Ended, RunEventKind::StatusChanged]
    );
    assert_eq!(reopened.events[1].prior_status, Some(RunStatus::Ended));
    assert_eq!(reopened.events[1].cause.as_deref(), Some("relaunched"));
    assert_eq!(status(&store, &first).0, RunStatus::Active);
    // A kept id with no row (a pre-Run record) is created.
    let adopted = RunId::mint();
    let made = ops::ensure_supervised(&store, &supervised(&adopted, true, false), AT).unwrap();
    assert!(made
        .events
        .iter()
        .any(|e| e.change == RunEventKind::Created));
}

#[test]
fn transitions_project_run_status_and_anomalies() {
    let (_tmp, store) = store();
    let a = name("a");
    // No open Run: nothing to project.
    let none = ops::project_transition(
        &store,
        &a,
        LifecycleState::Running,
        RunFate::Terminal,
        "x",
        AT,
    )
    .unwrap();
    assert!(none.events.is_empty());
    let run = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&run, true, true), AT).unwrap();
    let project =
        |state, fate| ops::project_transition(&store, &a, state, fate, "cause", AT).unwrap();
    assert!(project(LifecycleState::Running, RunFate::Terminal)
        .events
        .is_empty());
    assert!(project(LifecycleState::Starting, RunFate::Terminal)
        .events
        .is_empty());
    let held = project(LifecycleState::Paused, RunFate::Terminal);
    assert_eq!(held.events[0].change, RunEventKind::StatusChanged);
    assert_eq!(held.events[0].prior_status, Some(RunStatus::Active));
    assert_eq!(status(&store, &run).0, RunStatus::Held);
    // Stopping a paused agent is not a resume; a crash out of `paused` that
    // will relaunch only raises the Anomaly.
    assert!(project(LifecycleState::Stopping, RunFate::Terminal)
        .events
        .is_empty());
    project(LifecycleState::Failed, RunFate::Relaunching);
    assert_eq!(
        status(&store, &run),
        (RunStatus::Held, vec![Anomaly::Failed], None)
    );
    project(LifecycleState::Running, RunFate::Terminal);
    // A crash the Restart Policy relaunches: active + failed, no cascade.
    let crashed = project(LifecycleState::Failed, RunFate::Relaunching);
    assert_eq!(crashed.events.len(), 1);
    assert_eq!(crashed.events[0].change, RunEventKind::AnomaliesChanged);
    assert_eq!(
        status(&store, &run),
        (RunStatus::Active, vec![Anomaly::Failed], None)
    );
    project(LifecycleState::Starting, RunFate::Terminal);
    // Reaching running again clears it.
    project(LifecycleState::Running, RunFate::Terminal);
    assert_eq!(status(&store, &run), (RunStatus::Active, vec![], None));
    // A terminal crash ends the Run with the Anomaly and the cause.
    let ended = project(LifecycleState::Failed, RunFate::Terminal);
    assert_eq!(ended.events[0].change, RunEventKind::Ended);
    assert_eq!(
        status(&store, &run),
        (
            RunStatus::Ended,
            vec![Anomaly::Failed],
            Some("cause".to_string())
        )
    );
    // `registered` has no Run Status.
    assert!(RunStatus::projected_from(LifecycleState::Registered).is_none());
}

#[test]
fn stop_and_pause_cascade_to_in_process_children_only() {
    let (_tmp, store) = store();
    let a = name("a");
    let owner = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&owner, true, true), AT).unwrap();
    let c1 = child(&store, RunParent::Instance("a".to_string())).unwrap();
    let g1 = child(&store, RunParent::Run(c1.clone())).unwrap();
    let c2 = child(&store, RunParent::Run(owner.clone())).unwrap();
    ops::close(&store, &c2, AT).unwrap();
    let paused = ops::project_transition(
        &store,
        &a,
        LifecycleState::Paused,
        RunFate::Terminal,
        "pause",
        AT,
    )
    .unwrap();
    assert_eq!(paused.events.len(), 3, "owner + two open children");
    for id in [&c1, &g1] {
        assert_eq!(status(&store, id).0, RunStatus::Held);
    }
    assert_eq!(paused.events[1].cause.as_deref(), Some("parent-paused"));
    assert_eq!(
        status(&store, &c2).0,
        RunStatus::Ended,
        "a closed child stays ended"
    );
    let resumed = ops::project_transition(
        &store,
        &a,
        LifecycleState::Running,
        RunFate::Terminal,
        "resume",
        AT,
    )
    .unwrap();
    assert_eq!(resumed.events[1].cause.as_deref(), Some("parent-resumed"));
    for id in [&c1, &g1] {
        assert_eq!(status(&store, id).0, RunStatus::Active);
    }
    ops::project_transition(
        &store,
        &a,
        LifecycleState::Stopped,
        RunFate::Terminal,
        "stop",
        AT,
    )
    .unwrap();
    for id in [&c1, &g1] {
        assert_eq!(status(&store, id).2.as_deref(), Some("parent-stopped"));
    }
}

#[test]
fn a_terminal_failure_ends_children_as_parent_ended() {
    let (_tmp, store) = store();
    let owner = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&owner, true, true), AT).unwrap();
    let c = child(&store, RunParent::Run(owner.clone())).unwrap();
    ops::project_transition(
        &store,
        &name("a"),
        LifecycleState::Failed,
        RunFate::Terminal,
        "boom",
        AT,
    )
    .unwrap();
    assert_eq!(status(&store, &c).2.as_deref(), Some("parent-ended"));
}

#[test]
fn opening_a_child_is_refused_with_a_typed_cause() {
    let (_tmp, store) = store();
    assert!(matches!(
        child(&store, RunParent::Instance("a".to_string())),
        Err(RunError::NoOpenRun { .. })
    ));
    assert!(matches!(
        child(&store, RunParent::Instance("Not Valid".to_string())),
        Err(RunError::Invalid { .. })
    ));
    assert!(matches!(
        child(&store, RunParent::Run(RunId::from_wire("run_missing"))),
        Err(RunError::NotFound { .. })
    ));
    let owner = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&owner, false, true), AT).unwrap();
    // Nesting: the owner is level 1, so seven children fit beneath it.
    let mut parent = owner.clone();
    for _ in 0..7 {
        parent = child(&store, RunParent::Run(parent)).unwrap();
    }
    assert!(matches!(
        child(&store, RunParent::Run(parent.clone())),
        Err(RunError::NestingTooDeep {
            depth: 9,
            max: 8,
            ..
        })
    ));
    // A non-warn action at a depth that cannot execute it.
    for action in [BreachAction::Pause, BreachAction::Stop, BreachAction::Block] {
        let mut request = ChildRunRequest::under(RunParent::Run(owner.clone()));
        request.limits.breach_action = Some(action);
        match ops::open_child(&store, &request, RunId::mint(), AT) {
            Err(RunError::ActionUnavailable {
                metering_source, ..
            }) => assert_eq!(metering_source, "self-reported"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
    let mut request = ChildRunRequest::under(RunParent::Run(owner.clone()));
    request.labels = vec![("BAD".to_string(), "x".to_string())];
    assert!(matches!(
        ops::open_child(&store, &request, RunId::mint(), AT),
        Err(RunError::Invalid { .. })
    ));
    request.labels = vec![("k".into(), "1".into()), ("k".into(), "2".into())];
    assert!(matches!(
        ops::open_child(&store, &request, RunId::mint(), AT),
        Err(RunError::Invalid { .. })
    ));
    // An ended parent.
    let ended = child(&store, RunParent::Run(owner.clone())).unwrap();
    ops::close(&store, &ended, AT).unwrap();
    assert!(matches!(
        child(&store, RunParent::Run(ended)),
        Err(RunError::ParentEnded { .. })
    ));
}

#[test]
fn a_wire_child_refuses_hold_actions_until_they_exist_and_warn_is_accepted() {
    let (_tmp, store) = store();
    let owner = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&owner, true, true), AT).unwrap();
    let mut request = ChildRunRequest::under(RunParent::Run(owner.clone()));
    request.limits.breach_action = Some(BreachAction::Block);
    match ops::open_child(&store, &request, RunId::mint(), AT) {
        Err(err @ RunError::ActionUnavailable { .. }) => {
            assert!(err.to_string().contains("not available yet"), "{err}");
            assert_eq!(err.code(), "action-unavailable");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    request.limits.breach_action = Some(BreachAction::Warn);
    request.limits.token_per_run = Some(10);
    request.labels = vec![("task".into(), "review".into())];
    request.external_id = Some("job-7".into());
    let (id, changes) = ops::open_child(&store, &request, RunId::mint(), AT).unwrap();
    assert_eq!(changes.events[0].change, RunEventKind::Created);
    assert!(
        changes.events[0].labels.is_empty(),
        "caller labels are not bound"
    );
    let row = store.run_record(&id).unwrap().unwrap();
    assert_eq!(row.control_depth, ControlDepth::Announced);
    assert!(row.wire_path);
    assert_eq!(row.instance.as_deref(), Some("a"));
    // The same external id returns the same Run, changing nothing.
    let (again, changes) = ops::open_child(&store, &request, RunId::mint(), AT).unwrap();
    assert_eq!(again, id);
    assert!(changes.events.is_empty());
}

#[test]
fn closing_is_idempotent_and_refused_for_a_process_owner() {
    let (_tmp, store) = store();
    let owner = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&owner, true, true), AT).unwrap();
    assert!(matches!(
        ops::close(&store, &owner, AT),
        Err(RunError::OwnsProcess { .. })
    ));
    assert!(matches!(
        ops::close(&store, &RunId::from_wire("nope"), AT),
        Err(RunError::NotFound { .. })
    ));
    let c = child(&store, RunParent::Run(owner.clone())).unwrap();
    let g = child(&store, RunParent::Run(c.clone())).unwrap();
    assert_eq!(ops::close(&store, &c, AT).unwrap().events.len(), 1);
    assert!(ops::close(&store, &c, AT).unwrap().events.is_empty());
    assert_eq!(status(&store, &c).2.as_deref(), Some("closed"));
    assert_eq!(
        status(&store, &g).0,
        RunStatus::Active,
        "its children keep running"
    );
}

#[test]
fn a_depth_change_raises_and_clears_depth_fallback_for_a_set_action() {
    let (_tmp, store) = store();
    let owner = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&owner, true, true), AT).unwrap();
    let id = RunId::mint();
    // Seed a child carrying a directly set `pause` (as a later depth would
    // allow) and move its depth: the action cannot execute without a wire
    // path, so it falls back to `warn` with the Anomaly.
    store
        .insert_run(&crate::ports::NewRun {
            record: RunRecord {
                run_id: id.clone(),
                control_depth: ControlDepth::Announced,
                wire_path: true,
                status: RunStatus::Active,
                anomalies: vec![],
                parent_run_id: Some(owner.clone()),
                instance: Some("a".into()),
                process_owner: false,
                end_cause: None,
                created_at: AT.into(),
                updated_at: AT.into(),
                ended_at: None,
            },
            labels: vec![],
            limits: RunLimits {
                token_per_run: Some(5),
                dollar_per_run_micros: None,
                breach_action: Some(BreachAction::Pause),
            },
            identity: None,
        })
        .unwrap();
    let row = store.run_record(&id).unwrap().unwrap();
    let changes = ops::sync_depth(&store, row, ControlDepth::Announced, false, AT).unwrap();
    assert_eq!(changes.events[0].change, RunEventKind::DepthChanged);
    assert_eq!(status(&store, &id).1, vec![Anomaly::DepthFallback]);
    let row = store.run_record(&id).unwrap().unwrap();
    assert!(
        ops::sync_depth(&store, row.clone(), ControlDepth::Announced, false, AT)
            .unwrap()
            .events
            .is_empty()
    );
    // A depth that could execute it clears the fallback.
    ops::sync_depth(&store, row, ControlDepth::Supervised, true, AT).unwrap();
    assert!(status(&store, &id).1.is_empty());
}

#[test]
fn labels_sync_once_and_breach_marks_once() {
    let (_tmp, store) = store();
    let run = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&run, true, true), AT).unwrap();
    let same =
        ops::sync_bound_labels(&store, &run, &[Label::bound("team", "payments")], AT).unwrap();
    assert!(same.events.is_empty());
    let moved = ops::sync_bound_labels(&store, &run, &[Label::bound("team", "core")], AT).unwrap();
    assert_eq!(moved.events[0].change, RunEventKind::LabelsChanged);
    assert_eq!(moved.events[0].labels, vec![Label::bound("team", "core")]);
    assert!(ops::sync_bound_labels(
        &store,
        &RunId::from_wire("gone"),
        &[Label::bound("x", "y")],
        AT
    )
    .unwrap()
    .events
    .is_empty());
    assert_eq!(ops::mark_breach(&store, &run, AT).unwrap().events.len(), 1);
    assert!(ops::mark_breach(&store, &run, AT)
        .unwrap()
        .events
        .is_empty());
    assert!(ops::mark_breach(&store, &RunId::from_wire("gone"), AT)
        .unwrap()
        .events
        .is_empty());
}

#[test]
fn view_reads_subtree_usage_labels_limits_and_children() {
    let (_tmp, store) = store();
    let owner = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&owner, true, true), AT).unwrap();
    child(&store, RunParent::Run(owner.clone())).unwrap();
    let view = ops::view(&store, &owner).unwrap();
    assert_eq!(view.open_children, 1);
    assert_eq!(view.labels, vec![Label::bound("team", "payments")]);
    assert_eq!(view.usage.input_tokens, 0);
    assert!(matches!(
        ops::view(&store, &RunId::from_wire("nope")),
        Err(RunError::NotFound { .. })
    ));
    let json = serde_json::to_value(&view).unwrap();
    assert_eq!(json["control_depth"], "supervised");
    assert_eq!(json["status"], "active");
    assert_eq!(json["usage"]["estimate"], "estimated");
}

#[test]
fn a_committed_change_is_published_after_commit_and_a_rolled_back_one_never() {
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(&tmp.path().join("state.db")).unwrap();
    store.execute_batch_for_test(
        "INSERT INTO agent_instances (name, kind, state, agent_home, created_at, updated_at) \
         VALUES ('a', 'mock', 'running', '/h', 't', 't')",
    );
    let writer = StoreWriter::spawn(store).unwrap();
    let publisher = Arc::new(SnapshotPublisher::default());
    let run = RunId::mint();
    let p = Arc::clone(&publisher);
    let r = run.clone();
    let committed = writer
        .call(move |ctx| {
            let changes = ops::ensure_supervised(ctx.store(), &supervised(&r, true, true), AT)?;
            ops::commit(ctx, &p, changes)
        })
        .unwrap();
    assert_eq!(committed.len(), 1);
    assert!(committed[0].1.seq > 0, "the event is in the outbox");
    let snapshot = publisher.current();
    assert_eq!(snapshot.owner_of("a").unwrap().record.run_id, run);
    // A job that fails after registering its publish never publishes.
    let p = Arc::clone(&publisher);
    let failed: Result<(), crate::ports::StoreError> = writer.call(move |ctx| {
        let other = RunId::mint();
        let changes = ops::ensure_supervised(ctx.store(), &supervised(&other, true, true), AT)?;
        ops::commit(ctx, &p, changes)?;
        Err(crate::ports::StoreError::Backend("abort".to_string()))
    });
    assert!(failed.is_err());
    assert_eq!(
        publisher.current().owner_of("a").unwrap().record.run_id,
        run
    );
    let entries = writer.call(|ctx| ops::open_entries(ctx.store())).unwrap();
    assert_eq!(entries.len(), 1);
}

#[test]
fn the_wire_vocabulary_is_stable() {
    for depth in ControlDepth::ALL {
        assert_eq!(ControlDepth::from_wire(depth.as_str()), Some(depth));
        assert_eq!(
            serde_json::to_string(&depth).unwrap(),
            format!("\"{}\"", depth.as_str())
        );
    }
    assert_eq!(ControlDepth::from_wire("deep"), None);
    assert_eq!(
        ControlDepth::Supervised.display_with_wire(false),
        "supervised (no wire)"
    );
    assert_eq!(
        ControlDepth::Announced.display_with_wire(false),
        "announced"
    );
    for status in RunStatus::ALL {
        assert_eq!(RunStatus::from_wire(status.as_str()), Some(status));
    }
    assert_eq!(
        Anomaly::encode(&[Anomaly::Failed, Anomaly::Breach, Anomaly::Failed]),
        "breach,failed"
    );
    assert_eq!(
        Anomaly::decode("failed,unknown,breach"),
        vec![Anomaly::Breach, Anomaly::Failed]
    );
    assert_eq!(Anomaly::decode(""), Vec::<Anomaly>::new());
    assert_eq!(
        Anomaly::from_wire("depth-fallback"),
        Some(Anomaly::DepthFallback)
    );
    assert_eq!(LabelSource::from_wire("bound"), Some(LabelSource::Bound));
    assert_eq!(LabelSource::from_wire("caller"), Some(LabelSource::Caller));
    assert_eq!(LabelSource::from_wire("x"), None);
    assert_eq!(LabelSource::Caller.as_str(), "caller");
    for origin in [LabelOrigin::Config, LabelOrigin::LabelVerb] {
        assert_eq!(LabelOrigin::from_wire(origin.as_str()), Some(origin));
        assert_eq!(
            serde_json::to_string(&origin).unwrap(),
            format!("\"{}\"", origin.as_str())
        );
    }
    assert_eq!(LabelOrigin::from_wire("x"), None);
    assert_eq!(Label::caller("k", "v").bound_origin(), None);
    // A Caller label serializes no origin (additive on every surface).
    let json = serde_json::to_value(Label::caller("k", "v")).unwrap();
    assert!(json.get("origin").is_none(), "{json}");
    for kind in [
        RunEventKind::Created,
        RunEventKind::DepthChanged,
        RunEventKind::StatusChanged,
        RunEventKind::AnomaliesChanged,
        RunEventKind::LabelsChanged,
        RunEventKind::Ended,
    ] {
        assert_eq!(
            serde_json::to_string(&kind).unwrap(),
            format!("\"{}\"", kind.as_str())
        );
    }
    let limits = RunLimits::default();
    assert!(!limits.is_armed());
    assert_eq!(limits.action(), BreachAction::Warn);
    let codes = [
        RunError::NotFound { run_id: "x".into() }.code(),
        RunError::NoOpenRun {
            instance: "a".into(),
        }
        .code(),
        RunError::ParentEnded { run_id: "x".into() }.code(),
        RunError::NestingTooDeep {
            parent: "x".into(),
            depth: 9,
            max: 8,
        }
        .code(),
        RunError::OwnsProcess {
            run_id: "x".into(),
            instance: "a".into(),
        }
        .code(),
        RunError::Invalid {
            field: "f".into(),
            reason: "r".into(),
        }
        .code(),
        RunError::Store(crate::ports::StoreError::Backend("b".into())).code(),
        RunError::RunEnded { run_id: "x".into() }.code(),
        RunError::LabelFromConfig {
            run_id: "x".into(),
            key: "k".into(),
            instance: "a".into(),
        }
        .code(),
        RunError::LabelNotBound {
            run_id: "x".into(),
            key: "k".into(),
        }
        .code(),
        RunError::LabelBound {
            run_id: "x".into(),
            key: "k".into(),
        }
        .code(),
        RunError::TooManyRuns {
            selector: "a=b".into(),
            max: 1,
        }
        .code(),
    ];
    assert_eq!(
        codes,
        [
            "run-not-found",
            "no-open-run",
            "parent-ended",
            "nesting-too-deep",
            "owns-process",
            "invalid-request",
            "store",
            "run-ended",
            "label-from-config",
            "label-not-bound",
            "label-bound",
            "too-many-runs",
        ]
    );
}

#[test]
fn a_child_opened_under_a_held_parent_starts_held() {
    let (_tmp, store) = store();
    let owner = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&owner, true, true), AT).unwrap();
    ops::project_transition(
        &store,
        &name("a"),
        LifecycleState::Paused,
        RunFate::Terminal,
        "p",
        AT,
    )
    .unwrap();
    let (id, changes) = ops::open_child(
        &store,
        &ChildRunRequest::under(RunParent::Run(owner.clone())),
        RunId::mint(),
        AT,
    )
    .unwrap();
    assert_eq!(changes.events[0].cause.as_deref(), Some("parent-paused"));
    assert_eq!(status(&store, &id).0, RunStatus::Held);
    ops::project_transition(
        &store,
        &name("a"),
        LifecycleState::Running,
        RunFate::Terminal,
        "r",
        AT,
    )
    .unwrap();
    assert_eq!(status(&store, &id).0, RunStatus::Active);
}

#[test]
fn runs_of_agents_with_no_process_end_with_their_children() {
    let (_tmp, store) = store();
    let owner = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&owner, true, true), AT).unwrap();
    let c = child(&store, RunParent::Run(owner.clone())).unwrap();
    // `a` is running: nothing to end.
    assert!(ops::end_runs_without_a_process(&store, AT)
        .unwrap()
        .events
        .is_empty());
    store.execute_batch_for_test("UPDATE agent_instances SET state = 'failed' WHERE name = 'a'");
    let changes = ops::end_runs_without_a_process(&store, AT).unwrap();
    assert_eq!(changes.events.len(), 2);
    assert_eq!(
        status(&store, &owner).2.as_deref(),
        Some("relaunch-not-run")
    );
    assert_eq!(status(&store, &c).2.as_deref(), Some("parent-ended"));
}

// ---- Story 21-9: bound labels and the label verb ----

fn verb(
    store: &SqliteStore,
    run: &RunId,
    change: LabelChange,
    config: &[Label],
    dry_run: bool,
) -> Result<(LabelOutcome, ops::RunChanges), RunError> {
    ops::label(store, run, &change, config, dry_run, AT)
}

fn set(pairs: &[(&str, &str)]) -> LabelChange {
    LabelChange::set(pairs.iter().copied())
}

fn remove(keys: &[&str]) -> LabelChange {
    LabelChange::remove(keys.iter().copied())
}

#[test]
fn the_label_verb_binds_once_with_one_event_and_a_repeat_changes_nothing() {
    let (_tmp, store) = store();
    let run = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&run, true, true), AT).unwrap();
    let config = [Label::bound("team", "payments")];
    let (outcome, changes) = verb(&store, &run, set(&[("env", "prod")]), &config, false).unwrap();
    assert!(outcome.changed);
    assert!(!outcome.dry_run);
    assert_eq!(
        outcome.labels,
        vec![Label::verb("env", "prod"), Label::bound("team", "payments")]
    );
    assert_eq!(outcome.prior_labels, vec![Label::bound("team", "payments")]);
    assert_eq!(changes.events.len(), 1);
    let event = &changes.events[0];
    assert_eq!(event.change, RunEventKind::LabelsChanged);
    assert_eq!(event.cause.as_deref(), Some("label-verb"));
    assert_eq!(event.labels, outcome.labels);
    assert_eq!(
        event.prior_labels.as_deref(),
        Some(&[Label::bound("team", "payments")][..])
    );
    assert_eq!(changes.entries.len(), 1);
    assert_eq!(changes.entries[0].labels, outcome.labels);
    let json = serde_json::to_value(event).unwrap();
    assert_eq!(json["labels"][0]["origin"], "label-verb");
    assert_eq!(json["prior_labels"][0]["origin"], "config");
    // The same set again: unchanged, nothing emitted.
    let (again, changes) = verb(&store, &run, set(&[("env", "prod")]), &config, false).unwrap();
    assert!(!again.changed);
    assert!(changes.events.is_empty() && changes.entries.is_empty());
    assert_eq!(store.run_labels(&run).unwrap(), outcome.labels);
}

#[test]
fn a_dry_run_plans_and_writes_nothing() {
    let (_tmp, store) = store();
    let run = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&run, true, true), AT).unwrap();
    let before = store.run_labels(&run).unwrap();
    let (outcome, changes) = verb(&store, &run, set(&[("team", "core")]), &[], true).unwrap();
    assert!(outcome.changed && outcome.dry_run);
    assert_eq!(outcome.labels, vec![Label::verb("team", "core")]);
    assert!(changes.events.is_empty());
    assert_eq!(store.run_labels(&run).unwrap(), before);
}

#[test]
fn a_verb_label_outranks_config_across_syncs_and_its_removal_restores_config() {
    let (_tmp, store) = store();
    let run = RunId::mint();
    let config = [Label::bound("team", "payments")];
    ops::ensure_supervised(&store, &supervised(&run, true, true), AT).unwrap();
    verb(&store, &run, set(&[("team", "core")]), &config, false).unwrap();
    // A config write, a relaunch and an adoption all keep the verb's value.
    let synced = ops::sync_bound_labels(&store, &run, &config, AT).unwrap();
    assert!(synced.events.is_empty());
    let changed_config = [Label::bound("team", "other"), Label::bound("tier", "gold")];
    let synced = ops::sync_bound_labels(&store, &run, &changed_config, AT).unwrap();
    assert_eq!(synced.events.len(), 1, "tier is new");
    assert_eq!(synced.events[0].cause.as_deref(), Some("config"));
    let mut relaunch = supervised(&run, true, false);
    relaunch.labels = config.to_vec();
    ops::ensure_supervised(&store, &relaunch, AT).unwrap();
    assert_eq!(
        store.run_labels(&run).unwrap(),
        vec![Label::verb("team", "core")]
    );
    // Removing the verb label restores the configured value in one change.
    let (outcome, changes) = verb(&store, &run, remove(&["team"]), &config, false).unwrap();
    assert_eq!(outcome.labels, vec![Label::bound("team", "payments")]);
    assert_eq!(changes.events.len(), 1);
    assert_eq!(
        changes.events[0].prior_labels.as_deref(),
        Some(&[Label::verb("team", "core")][..])
    );
    // Now the key is bound by configuration only: removing it is refused,
    // naming the config key and the remediation.
    let refused = verb(&store, &run, remove(&["team"]), &config, false).unwrap_err();
    assert_eq!(refused.code(), "label-from-config");
    let message = refused.to_string();
    assert!(message.contains("labels.team"), "{message}");
    assert!(
        message.contains("hekma agent config set a labels.team"),
        "{message}"
    );
    assert_eq!(
        store.run_labels(&run).unwrap(),
        vec![Label::bound("team", "payments")]
    );
}

#[test]
fn the_verb_replaces_a_caller_value_but_never_removes_a_caller_label() {
    let (_tmp, store) = store();
    let owner = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&owner, true, true), AT).unwrap();
    let mut request = ChildRunRequest::under(RunParent::Run(owner.clone()));
    request.labels = vec![
        ("task".to_string(), "x".to_string()),
        ("flow".to_string(), "f".to_string()),
    ];
    let (child, _) = ops::open_child(&store, &request, RunId::mint(), AT).unwrap();
    // Configuration never reaches a child: the planner is given none.
    let config = [Label::bound("task", "cfg")];
    let (outcome, changes) = verb(&store, &child, set(&[("task", "y")]), &config, false).unwrap();
    assert!(outcome.prior_labels.contains(&Label::caller("task", "x")));
    assert!(outcome.labels.contains(&Label::verb("task", "y")));
    assert_eq!(changes.events[0].prior_labels.as_deref(), Some(&[][..]));
    let refused = verb(&store, &child, remove(&["flow"]), &[], false).unwrap_err();
    assert_eq!(refused.code(), "label-not-bound");
    let (absent, changes) = verb(&store, &child, remove(&["nope"]), &[], false).unwrap();
    assert!(!absent.changed);
    assert!(changes.events.is_empty());
    // Removing the verb label leaves no value (no config on a child).
    let (gone, _) = verb(&store, &child, remove(&["task"]), &config, false).unwrap();
    assert_eq!(gone.labels, vec![Label::caller("flow", "f")]);
}

#[test]
fn the_verb_refuses_ended_missing_runs_and_invalid_changes() {
    let (_tmp, store) = store();
    let owner = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&owner, true, true), AT).unwrap();
    let c = child(&store, RunParent::Run(owner.clone())).unwrap();
    ops::close(&store, &c, AT).unwrap();
    let ended = verb(&store, &c, set(&[("team", "core")]), &[], false).unwrap_err();
    assert_eq!(ended.code(), "run-ended");
    let missing = verb(
        &store,
        &RunId::from_wire("run_none"),
        set(&[("team", "core")]),
        &[],
        false,
    )
    .unwrap_err();
    assert_eq!(missing.code(), "run-not-found");
    let invalid = verb(&store, &owner, LabelChange::default(), &[], false).unwrap_err();
    assert_eq!(invalid.code(), "invalid-request");
}

#[test]
fn a_selector_matches_open_runs_by_any_label_in_run_id_order() {
    let (_tmp, store) = store();
    let owner = RunId::mint();
    ops::ensure_supervised(&store, &supervised(&owner, true, true), AT).unwrap();
    verb(&store, &owner, set(&[("env", "prod")]), &[], false).unwrap();
    let mut request = ChildRunRequest::under(RunParent::Run(owner.clone()));
    request.labels = vec![
        ("team".to_string(), "payments".to_string()),
        ("env".to_string(), "prod".to_string()),
    ];
    let (c1, _) = ops::open_child(&store, &request, RunId::mint(), AT).unwrap();
    let (c2, _) = ops::open_child(&store, &request, RunId::mint(), AT).unwrap();
    ops::close(&store, &c2, AT).unwrap();
    let selector = Selector::parse("team=payments,env=prod").unwrap();
    let mut expected = vec![owner.clone(), c1.clone()];
    expected.sort();
    let matched: Vec<RunId> = ops::select(&store, &selector)
        .unwrap()
        .into_iter()
        .map(|r| r.run_id)
        .collect();
    assert_eq!(matched, expected, "bound and Caller labels both match");
    let none = Selector::parse("team=nobody").unwrap();
    assert!(ops::select(&store, &none).unwrap().is_empty());
}

#[test]
fn a_selector_matching_more_than_the_cap_is_refused() {
    let (_tmp, store) = store();
    let mut sql = String::from("BEGIN;");
    for i in 0..=MAX_SELECTED_RUNS {
        sql.push_str(&format!(
            "INSERT INTO runs (run_id, control_depth, status, created_at, updated_at) \
             VALUES ('run-cap-{i:05}', 'announced', 'active', 't', 't'); \
             INSERT INTO run_labels (run_id, key, value, source) \
             VALUES ('run-cap-{i:05}', 'flow', 'bulk', 'caller');"
        ));
    }
    sql.push_str("COMMIT;");
    store.execute_batch_for_test(&sql);
    let selector = Selector::parse("flow=bulk").unwrap();
    let refused = ops::select(&store, &selector).unwrap_err();
    assert_eq!(refused.code(), "too-many-runs");
    assert!(refused.to_string().contains("1000"), "{refused}");
    // One fewer fits exactly.
    store.execute_batch_for_test("UPDATE runs SET status = 'ended' WHERE run_id = 'run-cap-00000'");
    assert_eq!(
        ops::select(&store, &selector).unwrap().len(),
        MAX_SELECTED_RUNS
    );
}

#[test]
fn every_bound_label_path_calls_the_policy_hook_inside_the_writer_job() {
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(&tmp.path().join("state.db")).unwrap();
    store.execute_batch_for_test(
        "INSERT INTO agent_instances (name, kind, state, agent_home, created_at, updated_at) \
         VALUES ('a', 'mock', 'running', '/h', 't', 't')",
    );
    let writer = StoreWriter::spawn(store).unwrap();
    let publisher = Arc::new(SnapshotPublisher::default());
    let run = RunId::mint();
    // Each operation runs as one writer job; the job returns the Runs whose
    // Policy hook ran inside it, plus the events it committed.
    type Op = Box<dyn FnOnce(&SqliteStore) -> ops::RunChanges + Send>;
    let job = |op: Op| -> (Vec<RunId>, usize) {
        let p = Arc::clone(&publisher);
        writer
            .call(move |ctx| {
                let changes = op(ctx.store());
                let resolved = changes.policy_resolved.clone();
                let events = ops::commit(ctx, &p, changes)?;
                Ok((resolved, events.len()))
            })
            .unwrap()
    };
    let r = run.clone();
    let (resolved, _) = job(Box::new(move |s| {
        ops::ensure_supervised(s, &supervised(&r, true, true), AT).unwrap()
    }));
    assert!(resolved.is_empty(), "creation is not a label change");
    // The verb.
    let r = run.clone();
    let (resolved, events) = job(Box::new(move |s| {
        ops::label(s, &r, &set(&[("env", "prod")]), &[], false, AT)
            .unwrap()
            .1
    }));
    assert_eq!((resolved, events), (vec![run.clone()], 1));
    // A config write's re-sync.
    let r = run.clone();
    let (resolved, _) = job(Box::new(move |s| {
        ops::sync_bound_labels(s, &r, &[Label::bound("team", "core")], AT).unwrap()
    }));
    assert_eq!(resolved, vec![run.clone()]);
    // A relaunch and an adoption (both keep the Run through
    // `ensure_supervised` with `fresh: false`) whose configuration moved.
    for value in ["other", "again"] {
        let r = run.clone();
        let (resolved, _) = job(Box::new(move |s| {
            let mut kept = supervised(&r, true, false);
            kept.labels = vec![Label::bound("team", value)];
            ops::ensure_supervised(s, &kept, AT).unwrap()
        }));
        assert_eq!(resolved, vec![run.clone()], "{value}");
    }
    // Nothing changed: no hook.
    let r = run.clone();
    let (resolved, events) = job(Box::new(move |s| {
        ops::sync_bound_labels(s, &r, &[Label::bound("team", "again")], AT).unwrap()
    }));
    assert!(resolved.is_empty() && events == 0);
    // A dry run never calls it.
    let r = run.clone();
    let (resolved, _) = job(Box::new(move |s| {
        ops::label(s, &r, &set(&[("env", "dev")]), &[], true, AT)
            .unwrap()
            .1
    }));
    assert!(resolved.is_empty());
    // The snapshot carries the verb label after commit.
    let snapshot = publisher.current();
    let entry = snapshot.run(&run).unwrap();
    assert!(entry.labels.contains(&Label::verb("env", "prod")));
}
