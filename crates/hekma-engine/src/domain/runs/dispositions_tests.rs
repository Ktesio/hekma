//! The disposition verbs' tables (story 23-4): the gate, the status flips,
//! each verb clearing only its own reason, and the reroute rules.

use super::*;
use crate::domain::runs::wire::{ensure_wire_run, Ensured, WireIdentity};
use crate::domain::runs::{ControlDepth, RunEventKind};
use crate::store::proxy_sqlite::{BindingRow, RouteRow};

fn store() -> SqliteStore {
    let store = SqliteStore::open_in_memory().unwrap();
    store
        .put_binding(&BindingRow {
            name: "openai".into(),
            prefix: "/openai".into(),
            upstream: "https://api.openai.com/v1".into(),
            custodied_key: None,
            labels: vec![("team".into(), "core".into())],
            created_at: "t".into(),
            updated_at: "t".into(),
        })
        .unwrap();
    for (route, upstream, key) in [
        ("backup", "https://backup.example.com/v1", None),
        ("same", "https://api.openai.com", None),
        ("keyed", "https://other.example.com", Some("secret:OTHER")),
    ] {
        store
            .put_binding_route(
                "openai",
                &RouteRow {
                    route: route.into(),
                    upstream: upstream.into(),
                    custodied_key: key.map(str::to_string),
                    created_at: "t".into(),
                },
            )
            .unwrap();
    }
    store
}

fn wire_run(store: &SqliteStore) -> RunId {
    match ensure_wire_run(
        store,
        &WireIdentity::Binding {
            binding: "openai".into(),
            labels: vec![("team".into(), "core".into())],
        },
        RunId::mint(),
        "t",
    )
    .unwrap()
    .0
    {
        Ensured::Run(run) => run,
        other => panic!("{other:?}"),
    }
}

fn act(
    store: &SqliteStore,
    run: &RunId,
    verb: DispositionVerb,
) -> (DispositionOutcome, RunChanges) {
    apply(store, run, &verb, "operator", "2026-11-14T00:00:00Z").unwrap()
}

#[test]
fn hold_and_release_flip_the_status_and_each_verb_clears_only_its_own_reason() {
    let store = store();
    let run = wire_run(&store);
    let (held, changes) = act(&store, &run, DispositionVerb::Hold);
    assert!(held.changed);
    assert_eq!(held.reasons, vec!["hold".to_string()]);
    let kinds: Vec<RunEventKind> = changes.events.iter().map(|e| e.change).collect();
    assert_eq!(
        kinds,
        vec![
            RunEventKind::StatusChanged,
            RunEventKind::DispositionChanged
        ]
    );
    assert_eq!(
        store.run_record(&run).unwrap().unwrap().status,
        RunStatus::Held
    );
    // Holding again changes nothing.
    let (again, quiet) = act(&store, &run, DispositionVerb::Hold);
    assert!(!again.changed && quiet.events.is_empty());
    // Reroute and reject stand beside it; release clears only the hold.
    act(
        &store,
        &run,
        DispositionVerb::Reroute(Some("openai/backup".into())),
    );
    act(&store, &run, DispositionVerb::Reject);
    let (released, _) = act(&store, &run, DispositionVerb::Release);
    assert!(released.changed);
    assert_eq!(
        released.reasons,
        vec!["reject".to_string(), "reroute openai/backup".to_string()]
    );
    assert_eq!(
        store.run_record(&run).unwrap().unwrap().status,
        RunStatus::Active
    );
    let (off, _) = act(&store, &run, DispositionVerb::Reroute(None));
    assert_eq!(off.reasons, vec!["reject".to_string()]);
    // Releasing with no hold changes nothing.
    assert!(!act(&store, &run, DispositionVerb::Release).0.changed);
    // The snapshot entry carries the reasons.
    let extras = store.entry_extras(&run).unwrap();
    assert!(extras.disposition.verb_reject && !extras.disposition.verb_hold);
}

#[test]
fn the_gate_refuses_observed_no_wire_ended_and_unknown_runs() {
    let store = store();
    let run = wire_run(&store);
    let mut record = store.run_record(&run).unwrap().unwrap();
    record.control_depth = ControlDepth::Observed;
    record.wire_path = false;
    store.save_run(&record).unwrap();
    let refused = apply(&store, &run, &DispositionVerb::Hold, "operator", "t").unwrap_err();
    assert!(matches!(refused, RunError::VerbUnavailable { .. }));
    assert!(refused.to_string().contains("hold"));
    record.control_depth = ControlDepth::Supervised;
    store.save_run(&record).unwrap();
    let refused = apply(&store, &run, &DispositionVerb::Reject, "operator", "t").unwrap_err();
    assert!(matches!(refused, RunError::VerbUnavailable { reason, .. } if reason.contains("skip")));
    record.status = RunStatus::Ended;
    record.ended_at = Some("t".into());
    store.save_run(&record).unwrap();
    assert!(matches!(
        apply(&store, &run, &DispositionVerb::Hold, "operator", "t"),
        Err(RunError::RunEnded { .. })
    ));
    assert!(matches!(
        apply(
            &store,
            &RunId::from_wire("run_NONE"),
            &DispositionVerb::Hold,
            "operator",
            "t"
        ),
        Err(RunError::NotFound { .. })
    ));
    assert_eq!(DispositionVerb::Release.as_str(), "release");
}

#[test]
fn a_reroute_needs_a_route_and_never_takes_a_held_key_off_its_origin() {
    let store = store();
    let run = wire_run(&store);
    // An unknown route, and a malformed reference.
    for bad in ["openai/nope", "nobinding"] {
        assert!(matches!(
            apply(
                &store,
                &run,
                &DispositionVerb::Reroute(Some(bad.into())),
                "operator",
                "t"
            ),
            Err(RunError::Invalid { .. })
        ));
    }
    // With custody pinned to api.openai.com: another origin is refused, the
    // same origin is fine, and a route with its own key is fine.
    store
        .set_run_custody(
            &run,
            Some(crate::ports::RunCustodyRow {
                state: "on".into(),
                reason: "held".into(),
                origin: Some("https://api.openai.com".into()),
                key_ref: Some("secret:OPENAI".into()),
            }),
        )
        .unwrap();
    let refused = apply(
        &store,
        &run,
        &DispositionVerb::Reroute(Some("openai/backup".into())),
        "operator",
        "t",
    )
    .unwrap_err();
    assert!(
        refused.to_string().contains("https://api.openai.com"),
        "{refused}"
    );
    for ok in ["openai/same", "openai/keyed"] {
        let (outcome, _) = act(&store, &run, DispositionVerb::Reroute(Some(ok.into())));
        assert!(outcome.reasons.iter().any(|r| r.ends_with(ok)));
    }
    // Re-setting the same route changes nothing; the target resolves.
    assert!(
        !act(
            &store,
            &run,
            DispositionVerb::Reroute(Some("openai/keyed".into()))
        )
        .0
        .changed
    );
    let target = store
        .entry_extras(&run)
        .unwrap()
        .disposition
        .reroute
        .unwrap();
    assert_eq!(target.custodied_key.as_deref(), Some("secret:OTHER"));
}
