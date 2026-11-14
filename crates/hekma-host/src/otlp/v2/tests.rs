//! `otlp-metrics v2` without I/O (story 24-5).

use super::*;
use hekma_engine::{
    BreachAction, BudgetBreachEvent, ControlDepth, EngineEvent, Micros, RunId, UsageEvent,
    UsageUpdateEvent,
};

fn usage(
    run: &str,
    instance: Option<&str>,
    depth: ControlDepth,
    labels: &[(&str, &str)],
) -> EngineEvent {
    let mut update = UsageUpdateEvent::new(UsageEvent {
        instance: instance.map(str::to_string),
        run_id: RunId::from_wire(run),
        call_id: None,
        input_tokens: 100,
        output_tokens: 20,
        cached_tokens: None,
        metering_source: "engine-observed".into(),
        sequence: 0,
        occurred_at: "t".into(),
    });
    update.control_depth = Some(depth);
    update.policies = vec!["prod".into()];
    update.labels = labels
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    EngineEvent::UsageUpdate(update)
}

fn json(state: &MetricStateV2) -> serde_json::Value {
    serde_json::to_value(state.snapshot(5)).unwrap()
}

fn metric<'a>(doc: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    doc["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == name)
}

fn attrs(point: &serde_json::Value) -> Vec<(String, String)> {
    point["attributes"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|kv| {
                    (
                        kv["key"].as_str().unwrap().to_string(),
                        kv["value"]["stringValue"].as_str().unwrap().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn v2_series_carry_depth_and_allow_listed_labels_and_never_a_run_id() {
    let mut state = MetricStateV2::new(1, "svc".into(), vec!["team".into()], 200);
    // A wire Run with no Agent Instance exports, under its depth.
    state.observe_at(
        &usage(
            "run_W",
            None,
            ControlDepth::Wire,
            &[("team", "core"), ("job", "7")],
        ),
        2,
    );
    state.observe_at(
        &usage("run_S", Some("bot"), ControlDepth::Supervised, &[]),
        3,
    );
    let doc = json(&state);
    assert_eq!(
        doc["resourceMetrics"][0]["scopeMetrics"][0]["scope"]["version"],
        "v2"
    );
    let text = doc.to_string();
    let points = metric(&doc, "gen_ai.client.token_usage").unwrap()["histogram"]["dataPoints"]
        .as_array()
        .unwrap();
    for point in points {
        assert!(
            attrs(point).iter().all(|(key, _)| key != "hekma.run_id"),
            "{point}"
        );
        assert!(attrs(point).iter().all(|(key, _)| key != "hekma.policy"));
        assert!(attrs(point).iter().all(|(key, _)| key != "hekma.label.job"));
    }
    let wire: Vec<_> = points
        .iter()
        .filter(|p| attrs(p).contains(&("hekma.control_depth".into(), "wire".into())))
        .collect();
    assert_eq!(wire.len(), 2, "input and output");
    assert!(attrs(wire[0]).contains(&("hekma.label.team".into(), "core".into())));
    assert!(attrs(wire[0])
        .iter()
        .all(|(key, _)| key != "hekma.instance"));
    assert!(points
        .iter()
        .any(|p| attrs(p).contains(&("hekma.instance".into(), "bot".into()))));
    // The exemplar: only the run id, as filteredAttributes.
    let exemplar = &wire[0]["exemplars"][0];
    assert_eq!(exemplar["filteredAttributes"][0]["key"], "hekma.run_id");
    assert_eq!(
        exemplar["filteredAttributes"][0]["value"]["stringValue"],
        "run_W"
    );
    assert_eq!(exemplar["filteredAttributes"].as_array().unwrap().len(), 1);
    assert!(exemplar["timeUnixNano"].is_string() && exemplar["asInt"].is_string());
    assert!(exemplar.get("traceId").is_none() && exemplar.get("spanId").is_none());
    assert!(
        !text.contains("\"key\":\"hekma.run_id\",\"value\":{\"stringValue\":\"run_W\"}}],\"start")
    );
    // The always-emitted families.
    for name in [
        "hekma.calls.unscented",
        "hekma.telemetry.received",
        "hekma.otlp.folded",
        "hekma.otlp.dropped",
    ] {
        assert!(metric(&doc, name).is_some(), "{name}");
    }
    let unscented = metric(&doc, "hekma.calls.unscented").unwrap()["sum"]["dataPoints"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(unscented, 2);
}

#[test]
fn exemplars_keep_the_eight_most_recent_distinct_runs() {
    let mut state = MetricStateV2::new(1, "svc".into(), Vec::new(), 200);
    for n in 0..20 {
        state.observe_at(
            &usage(&format!("run_{n:02}"), None, ControlDepth::Wire, &[]),
            10 + n,
        );
    }
    // A repeat moves its Run to the front without a duplicate.
    state.observe_at(&usage("run_15", None, ControlDepth::Wire, &[]), 100);
    let doc = json(&state);
    let point = &metric(&doc, "gen_ai.client.token_usage").unwrap()["histogram"]["dataPoints"][0];
    let runs: Vec<&str> = point["exemplars"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            e["filteredAttributes"][0]["value"]["stringValue"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert_eq!(
        runs,
        ["run_15", "run_19", "run_18", "run_17", "run_16", "run_14", "run_13", "run_12"]
    );
}

#[test]
fn a_breach_names_its_policy_and_values_past_the_cap_fold_and_count() {
    let mut state = MetricStateV2::new(1, "svc".into(), vec!["team".into()], 2);
    let breach = |run: &str, team: &str, policy: Option<&str>| {
        let mut event = BudgetBreachEvent::new_cost(
            "bot",
            run,
            hekma_engine::BreachScope::Cumulative,
            Micros(1_000_000),
            Micros(2_500_000),
            hekma_engine::EstimateLabel::Estimated,
            BreachAction::Warn,
            "engine-observed",
            "t",
        );
        event.control_depth = Some(ControlDepth::Supervised);
        event.policy = policy.map(str::to_string);
        event.labels = vec![("team".into(), team.into())];
        EngineEvent::BudgetBreach(event)
    };
    state.observe_at(&breach("run_A", "a", Some("prod")), 2);
    state.observe_at(&breach("run_B", "b", Some("explicit")), 3);
    state.observe_at(&breach("run_C", "c", None), 4);
    let doc = json(&state);
    let points = metric(&doc, "hekma.budget.breaches").unwrap()["sum"]["dataPoints"]
        .as_array()
        .unwrap();
    let policies: Vec<String> = points
        .iter()
        .flat_map(|p| {
            attrs(p)
                .into_iter()
                .filter(|(k, _)| k == "hekma.policy")
                .map(|(_, v)| v)
        })
        .collect();
    assert_eq!(policies.len(), 3);
    for expected in ["prod", "explicit", "default"] {
        assert!(policies.iter().any(|p| p == expected), "{policies:?}");
    }
    // The cap is two: `c` folded.
    assert!(points.iter().any(|p| attrs(p).contains(&(
        "hekma.label.team".into(),
        crate::otlp::attributes::OVERFLOW_VALUE.into()
    ))));
    let folded = &metric(&doc, "hekma.otlp.folded").unwrap()["sum"]["dataPoints"][0]["asInt"];
    assert_eq!(
        *folded, "1",
        "one breach observation folded (its cost point shares it)"
    );
    let notes = state.take_notes();
    assert!(notes.iter().any(|n| n.contains("\"team\"")), "{notes:?}");
    // The cost series keeps its mandatory estimate label.
    let cost = &metric(&doc, "hekma.usage.cost").unwrap()["sum"]["dataPoints"][0];
    assert!(attrs(cost).contains(&("hekma.estimate_state".into(), "estimated".into())));
    assert_eq!(cost["asDouble"], 2.5);
}

#[test]
fn the_exporter_state_switches_exposition_and_keeps_v1_whole() {
    let event = usage(
        "run_W",
        Some("bot"),
        ControlDepth::Wire,
        &[("team", "core")],
    );
    let mut v1 = ExporterState::new(Exposition::V1, 1, "svc".into(), Vec::new(), 200);
    let mut v2 = ExporterState::new(Exposition::V2, 1, "svc".into(), vec!["team".into()], 200);
    assert_eq!((v1.version(), v2.version()), ("v1", "v2"));
    for state in [&mut v1, &mut v2] {
        state.observe(&event);
        state.sample(ExportCounters {
            unscented_served: 3,
            unscented_refused: 1,
            telemetry_accepted: 7,
            telemetry_skipped: 2,
        });
        state.note_dropped_events(1);
        state.note_dropped_batch();
        state.note_rejected_points(2);
        assert_eq!(state.dropped(), 4);
        assert_eq!(state.in_flight(), 1);
        state.mark_delivered();
        assert_eq!(state.in_flight(), 0);
        assert!(state.take_notes().is_empty());
    }
    let v1_doc = serde_json::to_value(v1.snapshot(5)).unwrap().to_string();
    assert!(v1_doc.contains("hekma.run_id"), "v1 keeps the run id");
    assert!(!v1_doc.contains("exemplars") && !v1_doc.contains("hekma.control_depth"));
    let v2_doc = serde_json::to_value(v2.snapshot(5)).unwrap();
    let received = &metric(&v2_doc, "hekma.telemetry.received").unwrap()["sum"]["dataPoints"];
    assert_eq!(received[0]["asInt"], "7");
    assert_eq!(received[1]["asInt"], "2");
}

/// Story 22-4: a Run event naming a bypass detection.
fn detected(run: &str, cause: &str) -> EngineEvent {
    let mut event: hekma_engine::RunEvent = serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "change": "bypass-detected",
        "run_id": run,
        "control_depth": "supervised",
        "wire_path": true,
        "status": "active",
        "anomalies": ["bypass"],
        "labels": [],
        "cause": cause,
        "at": "t",
    }))
    .unwrap();
    event.labels = vec![hekma_engine::Label::bound("team", "core")];
    EngineEvent::Run(event)
}

#[test]
fn a_bypass_counts_by_host_with_the_run_as_exemplar_and_informational_ones_do_not() {
    let mut state = MetricStateV2::new(1, "svc".into(), vec!["team".into()], 200);
    let doc = json(&state);
    assert!(
        metric(&doc, "hekma.bypass.detected").is_none(),
        "nothing yet"
    );
    state.observe_at(&detected("run_A", "api.x.com"), 2);
    state.observe_at(&detected("run_B", "api.x.com"), 3);
    state.observe_at(&detected("run_C", "api.y.com (informational: no-wire)"), 4);
    let doc = json(&state);
    let points = metric(&doc, "hekma.bypass.detected").unwrap()["sum"]["dataPoints"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(points.len(), 1, "the informational one is not a bypass");
    let point = &points[0];
    let found = attrs(point);
    assert!(found.contains(&("hekma.provider.host".into(), "api.x.com".into())));
    assert!(found.contains(&("hekma.control_depth".into(), "supervised".into())));
    assert!(found.contains(&("hekma.label.team".into(), "core".into())));
    assert!(found.iter().all(|(key, _)| key != "hekma.run_id"));
    assert_eq!(point["asInt"], "2");
    let runs: Vec<&str> = point["exemplars"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            e["filteredAttributes"][0]["value"]["stringValue"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert_eq!(runs, vec!["run_B", "run_A"]);
}
