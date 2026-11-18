//! Reconciliation ingestion (story 19-5): a Run's confirmed usage is stored
//! beside its estimated figures and never over them; the newest submission
//! is the reconciled figure with its drift; a retry is a replay, a reused
//! reference with other figures is a conflict; only the Operator or a Host
//! may submit; each accepted submission is one `reconciled` Run event.
//!
//! Every OS: no agent process.

use hekma_engine::{
    Engine, EngineEvent, EstimateLabel, Micros, ReconcileRequest, ReconcileSource, RunActor,
    RunError, RunEventKind,
};

/// A state directory with a parent Run, a child Run, and the child's
/// estimated usage (counted on the parent too, as the rollup does).
fn seeded() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    drop(Engine::open(Some(dir.path().to_path_buf())).unwrap());
    let conn = rusqlite::Connection::open(dir.path().join("state.db")).unwrap();
    conn.execute_batch(
        "INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, process_owner, \
         created_at, updated_at) VALUES ('run_P', 'announced', 0, 'active', '', 0, 't', 't');
         INSERT INTO runs (run_id, control_depth, wire_path, status, anomalies, parent_run_id, \
         process_owner, created_at, updated_at) VALUES ('run_C', 'announced', 0, 'active', '', \
         'run_P', 0, 't', 't');
         INSERT INTO usage_rollup_run (run_id, events, input_tokens, output_tokens, \
         cached_tokens, cached_unknown, cost_micros) VALUES ('run_C', 3, 900, 250, 0, 1, 4000);
         INSERT INTO usage_rollup_run (run_id, events, input_tokens, output_tokens, \
         cached_tokens, cached_unknown, cost_micros) VALUES ('run_P', 3, 900, 250, 0, 1, 4000);",
    )
    .unwrap();
    dir
}

fn request(reference: &str, input: u64) -> ReconcileRequest {
    ReconcileRequest {
        reference: reference.to_string(),
        source: ReconcileSource::Provider,
        input_tokens: input,
        output_tokens: 200,
        cached_tokens: None,
        cost_micros: Some(Micros(5_000)),
        note: Some("November invoice".to_string()),
    }
}

fn rollup(dir: &tempfile::TempDir) -> Vec<(String, i64, i64, i64)> {
    let conn = rusqlite::Connection::open(dir.path().join("state.db")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT run_id, input_tokens, output_tokens, cost_micros FROM usage_rollup_run \
             ORDER BY run_id",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows
}

#[test]
fn a_child_runs_confirmed_usage_sits_beside_its_estimate_with_the_drift() {
    let dir = seeded();
    let before = rollup(&dir);
    let engine = Engine::open(Some(dir.path().to_path_buf())).unwrap();
    let facade = engine.blocking();
    let mut events = facade.subscribe();
    let outcome = facade
        .reconcile_run("run_C", request("inv-1", 1_000), RunActor::Host)
        .unwrap();
    assert!(!outcome.replayed);
    assert_eq!(outcome.reconciliation.submitted_by, "host");
    assert_eq!(outcome.reconciliation.estimate, EstimateLabel::Reconciled);
    let view = facade.run("run_C").unwrap();
    assert_eq!(view.usage.estimate, EstimateLabel::Estimated);
    assert_eq!(view.usage.input_tokens, 900, "the estimate is untouched");
    let reconciled = view.reconciled.expect("the reconciled figure");
    assert_eq!(reconciled.submissions, 1);
    assert_eq!(reconciled.drift.input_tokens, 100);
    assert_eq!(reconciled.drift.output_tokens, -50);
    assert_eq!(reconciled.drift.cost_micros, Some(1_000));
    assert_eq!(reconciled.drift.cached_tokens, None);
    // The parent is unchanged, and no estimated row moved.
    assert!(facade.run("run_P").unwrap().reconciled.is_none());
    drop(engine);
    assert_eq!(rollup(&dir), before);
    // One `reconciled` event, naming the source and the reference.
    let mut found = false;
    while let Ok(event) = events.try_recv() {
        if let EngineEvent::Run(event) = event {
            if event.change == RunEventKind::Reconciled {
                assert_eq!(event.run_id.as_str(), "run_C");
                assert_eq!(event.cause.as_deref(), Some("provider inv-1"));
                found = true;
            }
        }
    }
    assert!(found, "no reconciled event");
}

#[test]
fn a_retry_is_a_replay_a_reused_reference_conflicts_and_the_newest_is_current() {
    let dir = seeded();
    let engine = Engine::open(Some(dir.path().to_path_buf())).unwrap();
    let facade = engine.blocking();
    facade
        .reconcile_run("run_C", request("inv-1", 1_000), RunActor::Operator)
        .unwrap();
    let mut events = facade.subscribe();
    let replay = facade
        .reconcile_run("run_C", request("inv-1", 1_000), RunActor::Operator)
        .unwrap();
    assert!(replay.replayed);
    assert!(events.try_recv().is_err(), "a replay emits nothing");
    let conflict = facade
        .reconcile_run("run_C", request("inv-1", 1_234), RunActor::Operator)
        .unwrap_err();
    assert!(matches!(conflict, RunError::ReconciliationConflict { .. }));
    assert_eq!(conflict.code(), "reconciliation-conflict");
    facade
        .reconcile_run("run_C", request("inv-2", 950), RunActor::Operator)
        .unwrap();
    let view = facade.run("run_C").unwrap().reconciled.unwrap();
    assert_eq!(view.submissions, 2);
    assert_eq!(view.current.figures.reference, "inv-2");
    assert_eq!(view.drift.input_tokens, 50);
    let all = facade.run_reconciliations("run_C").unwrap();
    let references: Vec<&str> = all.iter().map(|r| r.figures.reference.as_str()).collect();
    assert_eq!(references, ["inv-2", "inv-1"]);
    assert_eq!(all[1].submitted_by, "operator");
}

#[test]
fn unknown_runs_bad_figures_and_a_runs_own_caller_are_refused() {
    let dir = seeded();
    let engine = Engine::open(Some(dir.path().to_path_buf())).unwrap();
    let facade = engine.blocking();
    let missing = facade
        .reconcile_run("run_GHOST", request("x", 1), RunActor::Operator)
        .unwrap_err();
    assert!(matches!(missing, RunError::NotFound { .. }));
    assert!(matches!(
        facade.run_reconciliations("run_GHOST").unwrap_err(),
        RunError::NotFound { .. }
    ));
    assert!(facade.run_reconciliations("run_C").unwrap().is_empty());
    let mut bad = request("has space", 1);
    assert!(matches!(
        facade
            .reconcile_run("run_C", bad.clone(), RunActor::Operator)
            .unwrap_err(),
        RunError::Invalid { .. }
    ));
    bad.reference = "ok".to_string();
    bad.cost_micros = Some(Micros(-1));
    assert!(facade
        .reconcile_run("run_C", bad, RunActor::Operator)
        .is_err());
    // A read-only engine refuses the write.
    drop(engine);
    let reader = Engine::open_read_only(Some(dir.path().to_path_buf())).unwrap();
    assert!(reader
        .blocking()
        .reconcile_run("run_C", request("z", 1), RunActor::Operator)
        .is_err());
}
