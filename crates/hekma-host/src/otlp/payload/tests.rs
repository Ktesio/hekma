//! The `otlp-metrics v1` exposition pinned WITHOUT I/O: every metric family,
//! every edge attribute from the I/O matrix, and the wire-mapping rules
//! (camelCase, decimal-string 64-bit ints, numeric temporality, fixed
//! histogram bounds, the mandatory estimate label) — as real unit tests.

use hekma_engine::{
    BreachAction, BreachScope, EngineEvent, EstimateLabel, LifecycleState, Micros, RunId,
    TransitionCause, TransitionEvent, UsageEvent, UsageUpdateEvent, MICROS_PER_DOLLAR,
};
use serde_json::Value;

use super::{MetricState, TokenType, MAX_RUNS_PER_INSTANCE, TOKEN_HISTOGRAM_BOUNDS};
use crate::otlp::payload::{AGGREGATION_TEMPORALITY_CUMULATIVE, EXPOSITION_VERSION, SCOPE_NAME};

fn transition(instance: &str, prior: LifecycleState, new: LifecycleState) -> EngineEvent {
    EngineEvent::Transition(TransitionEvent::new(
        instance,
        prior,
        new,
        TransitionCause::StopGraceful,
        "2026-10-25T00:00:00Z",
    ))
}

fn usage(instance: &str, input: u64, output: u64, cached: Option<u64>) -> EngineEvent {
    usage_in_run(instance, "run-1", input, output, cached)
}

fn usage_in_run(
    instance: &str,
    run: &str,
    input: u64,
    output: u64,
    cached: Option<u64>,
) -> EngineEvent {
    EngineEvent::UsageUpdate(UsageUpdateEvent::new(UsageEvent {
        instance: instance.to_string(),
        run_id: RunId::from_wire(run),
        input_tokens: input,
        output_tokens: output,
        cached_tokens: cached,
        metering_source: "self-reported".to_string(),
        sequence: 0,
        occurred_at: "2026-10-25T00:00:00Z".to_string(),
    }))
}

fn dollar_breach(
    instance: &str,
    observed_micros: i64,
    label: EstimateLabel,
    scope: BreachScope,
    action: BreachAction,
) -> EngineEvent {
    EngineEvent::BudgetBreach(hekma_engine::BudgetBreachEvent::new_cost(
        instance,
        "run-1",
        scope,
        Micros(100 * MICROS_PER_DOLLAR),
        Micros(observed_micros),
        label,
        action,
        "self-reported",
        "2026-10-25T00:00:00Z",
    ))
}

fn token_breach(instance: &str, action: BreachAction) -> EngineEvent {
    EngineEvent::BudgetBreach(hekma_engine::BudgetBreachEvent::new(
        instance,
        "run-1",
        BreachScope::Cumulative,
        90,
        90,
        action,
        "self-reported",
        "2026-10-25T00:00:00Z",
    ))
}

fn state() -> MetricState {
    MetricState::new(1_000, "instance-id-1".to_string())
}

fn metric<'a>(payload: &'a Value, name: &str) -> &'a Value {
    payload["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .expect("metrics array")
        .iter()
        .find(|m| m["name"] == name)
        .unwrap_or_else(|| panic!("metric {name} absent from {payload}"))
}

#[test]
fn the_wire_mapping_is_camelcase_with_decimal_string_ints() {
    let mut state = state();
    state.observe(&usage("agent", 1_500, 200, None));
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let resource = &body["resourceMetrics"][0]["resource"];
    assert_eq!(resource["attributes"][0]["key"], "service.name");
    assert_eq!(
        resource["attributes"][0]["value"]["stringValue"],
        "hekma-host"
    );
    let scope = &body["resourceMetrics"][0]["scopeMetrics"][0];
    assert_eq!(scope["scope"]["name"], SCOPE_NAME);
    assert_eq!(scope["scope"]["version"], EXPOSITION_VERSION);
    let point = &metric(&body, "gen_ai.client.token_usage")["histogram"]["dataPoints"][0];
    // camelCase keys, decimal-STRING 64-bit ints.
    assert!(point.get("timeUnixNano").is_some(), "{point}");
    assert_eq!(point["timeUnixNano"], "2000");
    assert_eq!(point["startTimeUnixNano"], "1000");
    assert_eq!(point["count"], "1");
    assert!(point["bucketCounts"]
        .as_array()
        .unwrap()
        .iter()
        .all(|b| b.is_string()));
    // sum/min/max/explicitBounds are JSON NUMBERS.
    assert!(point["sum"].is_number());
    assert!(point["min"].is_number());
    assert!(point["max"].is_number());
    assert!(point["explicitBounds"]
        .as_array()
        .unwrap()
        .iter()
        .all(|b| b.is_number()));
}

#[test]
fn the_resource_names_the_host_its_version_and_the_state_dir_identity() {
    let body = serde_json::to_value(state().snapshot(2_000)).unwrap();
    let attrs = &body["resourceMetrics"][0]["resource"]["attributes"];
    let find = |key: &str| {
        attrs
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == key)
            .unwrap_or_else(|| panic!("{key} absent"))["value"]["stringValue"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(find("service.name"), "hekma-host");
    assert_eq!(find("service.instance.id"), "instance-id-1");
    assert_eq!(find("service.version"), env!("CARGO_PKG_VERSION"));
}

#[test]
fn token_usage_yields_one_point_per_known_token_type_with_the_semconv_attribute() {
    let mut state = state();
    state.observe(&usage("agent", 1_500, 200, Some(300)));
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let family = metric(&body, "gen_ai.client.token_usage");
    assert_eq!(family["unit"], "{token}");
    let points = family["histogram"]["dataPoints"].as_array().unwrap();
    assert_eq!(points.len(), 3, "input + output + cached observations");
    let token_type = |point: &Value| -> String {
        point["attributes"][0]["value"]["stringValue"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(token_type(&points[0]), TokenType::Input.as_str());
    assert_eq!(token_type(&points[1]), TokenType::Output.as_str());
    assert_eq!(token_type(&points[2]), TokenType::Cached.as_str());
    // Every point carries the instance/run attributes beside the type.
    for point in points {
        let keys: Vec<&str> = point["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["key"].as_str().unwrap())
            .collect();
        assert!(keys.contains(&"hekma.instance"));
        assert!(keys.contains(&"hekma.run_id"));
    }
    // The cached point is the REAL known subset (300), not a fabricated zero.
    let cached = &points[2];
    assert_eq!(cached["sum"], 300.0);
}

#[test]
fn an_unknown_cached_count_yields_no_cached_observation() {
    let mut state = state();
    state.observe(&usage("agent", 1_500, 200, None));
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let points = metric(&body, "gen_ai.client.token_usage")["histogram"]["dataPoints"]
        .as_array()
        .unwrap();
    assert_eq!(
        points.len(),
        2,
        "input + output only — never a fabricated cached zero"
    );
}

#[test]
fn the_histogram_buckets_use_the_documented_fixed_bounds() {
    assert_eq!(
        TOKEN_HISTOGRAM_BOUNDS,
        [1_000.0, 10_000.0, 100_000.0, 1_000_000.0, 10_000_000.0]
    );
    let mut state = state();
    // 1_500 tokens lands in the SECOND bucket (<= 10_000); six bucket counts.
    state.observe(&usage("agent", 1_500, 0, None));
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let point = &metric(&body, "gen_ai.client.token_usage")["histogram"]["dataPoints"][0];
    let buckets: Vec<u64> = point["bucketCounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(buckets.len(), TOKEN_HISTOGRAM_BOUNDS.len() + 1);
    assert_eq!(buckets, vec![0, 1, 0, 0, 0, 0], "1_500 <= 10_000");
    assert_eq!(
        point["explicitBounds"],
        Value::from(TOKEN_HISTOGRAM_BOUNDS.to_vec())
    );
}

#[test]
fn budget_breaches_sum_carry_dimension_scope_action_and_cumulative_temporality() {
    let mut state = state();
    state.observe(&token_breach("agent", BreachAction::Pause));
    state.observe(&token_breach("agent", BreachAction::Pause));
    state.observe(&token_breach("agent", BreachAction::Warn));
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let family = metric(&body, "hekma.budget.breaches");
    assert_eq!(family["unit"], "{breach}");
    let sum = &family["sum"];
    assert_eq!(
        sum["aggregationTemporality"], AGGREGATION_TEMPORALITY_CUMULATIVE,
        "numeric 1 = CUMULATIVE on sums"
    );
    assert_eq!(sum["isMonotonic"], true);
    let points = sum["dataPoints"].as_array().unwrap();
    assert_eq!(points.len(), 2, "one series per attribute combination");
    let find_point = |action: &str| {
        points
            .iter()
            .find(|p| {
                p["attributes"].as_array().unwrap().iter().any(|a| {
                    a["key"] == "hekma.budget.action" && a["value"]["stringValue"] == action
                })
            })
            .unwrap()
    };
    assert_eq!(find_point("pause")["asInt"], "2");
    assert_eq!(find_point("warn")["asInt"], "1");
    let pause = find_point("pause");
    let keys: Vec<&str> = pause["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["key"].as_str().unwrap())
        .collect();
    for expected in [
        "hekma.instance",
        "hekma.budget.dimension",
        "hekma.budget.scope",
        "hekma.budget.action",
    ] {
        assert!(keys.contains(&expected), "{expected} missing from {keys:?}");
    }
}

#[test]
fn a_dollar_breach_feeds_the_cost_sum_with_the_mandatory_estimate_state() {
    let mut state = state();
    state.observe(&dollar_breach(
        "agent",
        54_600_000,
        EstimateLabel::Estimated,
        BreachScope::Cumulative,
        BreachAction::Pause,
    ));
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let family = metric(&body, "hekma.usage.cost");
    assert_eq!(family["unit"], "USD");
    let point = &family["sum"]["dataPoints"][0];
    let find = |key: &str| {
        point["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == key)
            .unwrap_or_else(|| panic!("{key} missing — an unlabeled dollar is a test failure"))
            ["value"]["stringValue"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(find("hekma.estimate_state"), "estimated");
    // asDouble = micros / 1e6; asInt is absent on a double point.
    assert_eq!(point["asDouble"], 54.6);
    assert!(point.get("asInt").is_none(), "{point}");
    assert_eq!(point["timeUnixNano"], "2000");
}

#[test]
fn a_reconciled_figure_is_a_new_series_never_a_replacement() {
    let mut state = state();
    state.observe(&dollar_breach(
        "agent",
        54_600_000,
        EstimateLabel::Estimated,
        BreachScope::Cumulative,
        BreachAction::Pause,
    ));
    state.observe(&dollar_breach(
        "agent",
        9_000_000,
        EstimateLabel::Reconciled,
        BreachScope::Cumulative,
        BreachAction::Warn,
    ));
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let points = metric(&body, "hekma.usage.cost")["sum"]["dataPoints"]
        .as_array()
        .unwrap();
    assert_eq!(
        points.len(),
        2,
        "estimated and reconciled are SEPARATE series"
    );
    let labeled = |label: &str| {
        points
            .iter()
            .find(|p| {
                p["attributes"].as_array().unwrap().iter().any(|a| {
                    a["key"] == "hekma.estimate_state" && a["value"]["stringValue"] == label
                })
            })
            .unwrap()
    };
    assert_eq!(labeled("estimated")["asDouble"], 54.6);
    assert_eq!(labeled("reconciled")["asDouble"], 9.0);
}

#[test]
fn the_cost_series_takes_the_cumulative_max_and_never_regresses() {
    let mut state = state();
    // A PER-RUN breach first: it trips the breach counter but does NOT feed
    // the cost series (P1a) — so the 40.0 reading below is not "maxed
    // against", it never entered.
    state.observe(&dollar_breach(
        "agent",
        40_000_000,
        EstimateLabel::Estimated,
        BreachScope::PerRun,
        BreachAction::Pause,
    ));
    state.observe(&dollar_breach(
        "agent",
        60_000_000,
        EstimateLabel::Estimated,
        BreachScope::Cumulative,
        BreachAction::Stop,
    ));
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let points = metric(&body, "hekma.usage.cost")["sum"]["dataPoints"]
        .as_array()
        .unwrap();
    assert_eq!(points.len(), 1, "one estimated series for the instance");
    assert_eq!(
        points[0]["asDouble"], 60.0,
        "the cumulative reading, not a sum of breaches"
    );
}

#[test]
fn a_per_run_dollar_breach_counts_the_counter_but_never_feeds_the_cost_series() {
    let mut state = state();
    state.observe(&dollar_breach(
        "agent",
        54_600_000,
        EstimateLabel::Estimated,
        BreachScope::PerRun,
        BreachAction::Pause,
    ));
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let names: Vec<&str> = body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(
        !names.contains(&"hekma.usage.cost"),
        "a per-run observed total is a RUN window, not the lifetime \
         cumulative — no cost series from it: {names:?}"
    );
    // The breach itself is still counted, with its scope attribute intact.
    let breach = metric(&body, "hekma.budget.breaches");
    let point = &breach["sum"]["dataPoints"][0];
    assert_eq!(point["asInt"], "1");
    let scope = point["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == "hekma.budget.scope")
        .expect("the scope attribute stays on the breach counter");
    assert_eq!(scope["value"]["stringValue"], "per-run");
}

#[test]
fn a_dollar_breach_without_an_observed_total_emits_no_cost_point() {
    let mut state = state();
    let event = match dollar_breach(
        "agent",
        54_600_000,
        EstimateLabel::Estimated,
        BreachScope::Cumulative,
        BreachAction::Pause,
    ) {
        EngineEvent::BudgetBreach(mut event) => {
            event.dollar_observed = None;
            EngineEvent::BudgetBreach(event)
        }
        other => panic!("unexpected event {other:?}"),
    };
    state.observe(&event);
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let names: Vec<&str> = body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(
        !names.contains(&"hekma.usage.cost"),
        "an absent observed total is the honest absence — never a 0.0 \
         series: {names:?}"
    );
}

#[test]
fn agent_state_gauge_marks_the_entered_and_left_states() {
    let mut state = state();
    state.observe(&transition(
        "agent",
        LifecycleState::Running,
        LifecycleState::Stopped,
    ));
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let family = metric(&body, "hekma.agent.state");
    let points = family["gauge"]["dataPoints"].as_array().unwrap();
    assert_eq!(points.len(), 2);
    let value_of =
        |state_name: &str| {
            points
                .iter()
                .find(|p| {
                    p["attributes"].as_array().unwrap().iter().any(|a| {
                        a["key"] == "hekma.state" && a["value"]["stringValue"] == state_name
                    })
                })
                .unwrap_or_else(|| panic!("no point for state {state_name}"))["asInt"]
                .as_str()
                .unwrap()
                .to_string()
        };
    assert_eq!(value_of("running"), "0", "the state it LEFT");
    assert_eq!(value_of("stopped"), "1", "the state it ENTERED");
}

#[test]
fn the_dropped_self_metric_is_always_present_even_at_zero() {
    let body = serde_json::to_value(state().snapshot(2_000)).unwrap();
    let point = &metric(&body, "hekma.otlp.dropped")["sum"]["dataPoints"][0];
    assert_eq!(
        point["asInt"], "0",
        "always exported — a silent counter is useless"
    );
    let mut state = state();
    state.note_dropped_events(7);
    state.note_dropped_batch();
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let point = &metric(&body, "hekma.otlp.dropped")["sum"]["dataPoints"][0];
    assert_eq!(point["asInt"], "8");
    assert_eq!(state.dropped(), 8);
}

#[test]
fn an_empty_state_still_produces_a_valid_batch_carrying_the_dropped_metric() {
    let body = serde_json::to_value(state().snapshot(2_000)).unwrap();
    let metrics = body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap();
    assert_eq!(metrics.len(), 1, "only the dropped self-metric");
    assert_eq!(metrics[0]["name"], "hekma.otlp.dropped");
}

#[test]
fn the_operation_duration_metric_is_never_fabricated() {
    let mut state = state();
    state.observe(&usage("agent", 10, 10, None));
    state.observe(&token_breach("agent", BreachAction::Warn));
    state.observe(&transition(
        "agent",
        LifecycleState::Running,
        LifecycleState::Stopped,
    ));
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let names: Vec<&str> = body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(
        !names.iter().any(|n| n.contains("operation_duration")),
        "no producer exists for per-call duration — absent, never fabricated: {names:?}"
    );
}

#[test]
fn payloads_carry_no_content_and_no_secret_shaped_values() {
    // The no-leak matrix, payload-side: every attribute VALUE across a
    // fully-observed batch is from the frozen vocabulary (identifiers,
    // budget words, estimate labels, states, types) — never free content.
    let mut state = state();
    state.observe(&usage("agent", 10, 10, Some(1)));
    state.observe(&dollar_breach(
        "agent",
        1_000_000,
        EstimateLabel::Estimated,
        BreachScope::Cumulative,
        BreachAction::Pause,
    ));
    state.observe(&transition(
        "agent",
        LifecycleState::Registered,
        LifecycleState::Starting,
    ));
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let values = collect_string_values(&body);
    for value in values {
        assert!(
            value.len() < 128,
            "an attribute value is suspiciously long (content?): {value:?}"
        );
        assert!(
            !value.contains("secret:"),
            "a secret REFERENCE never rides the payload: {value:?}"
        );
    }
}

fn collect_string_values(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items.iter().flat_map(collect_string_values).collect(),
        Value::Object(map) => map
            .iter()
            .filter(|(key, _)| key.as_str() != "key")
            .flat_map(|(_, v)| collect_string_values(v))
            .collect(),
        _ => Vec::new(),
    }
}

#[test]
fn in_flight_tracks_observations_until_marked_delivered() {
    let mut state = state();
    assert_eq!(state.in_flight(), 0);
    state.observe(&usage("agent", 10, 10, None));
    state.observe(&transition(
        "agent",
        LifecycleState::Running,
        LifecycleState::Stopped,
    ));
    assert_eq!(state.in_flight(), 2, "observed but not yet delivered");
    state.mark_delivered();
    assert_eq!(
        state.in_flight(),
        0,
        "a successful POST clears the in-flight count"
    );
    state.observe(&usage("agent", 1, 1, None));
    assert_eq!(state.in_flight(), 1);
}

#[test]
fn a_backward_clock_never_inverts_the_cumulative_window() {
    // The flush moment is guarded: time = max(now, start) — a clock stepped
    // back past process start clamps at the window's open instead of
    // exporting start > end (P1c).
    let mut state = MetricState::new(5_000, "instance-id-1".to_string());
    state.observe(&usage("agent", 10, 10, None));
    let body = serde_json::to_value(state.snapshot(4_000)).unwrap();
    let point = &metric(&body, "gen_ai.client.token_usage")["histogram"]["dataPoints"][0];
    assert_eq!(point["startTimeUnixNano"], "5000");
    assert_eq!(
        point["timeUnixNano"], "5000",
        "a backward clock clamps at the window's open — never start > end"
    );
    // The normal forward direction is untouched.
    let body = serde_json::to_value(state.snapshot(6_000)).unwrap();
    let point = &metric(&body, "gen_ai.client.token_usage")["histogram"]["dataPoints"][0];
    assert_eq!(point["timeUnixNano"], "6000");
}

#[test]
fn the_run_series_window_keeps_the_most_recent_runs_per_instance() {
    let mut state = state();
    // MAX_RUNS_PER_INSTANCE runs, then ONE more: the first run (the least
    // recently observed) is evicted; the recency-refresher keeps its holder.
    for index in 0..=MAX_RUNS_PER_INSTANCE as u64 {
        state.observe(&usage_in_run(
            "agent",
            &format!("run-{index:03}"),
            100,
            100,
            None,
        ));
    }
    let evicted = state.take_evicted_run_series();
    assert_eq!(
        evicted,
        vec![("agent".to_string(), 1)],
        "exactly the one overflowed run, named per instance"
    );
    // The drain resets: no repeat count for the next cycle.
    assert!(state.take_evicted_run_series().is_empty());
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let points = metric(&body, "gen_ai.client.token_usage")["histogram"]["dataPoints"]
        .as_array()
        .unwrap();
    assert_eq!(
        points.len(),
        MAX_RUNS_PER_INSTANCE * 2,
        "input + output series for each KEPT run (memory and payload bounded)"
    );
    let run_ids: Vec<String> = points
        .iter()
        .map(|p| {
            p["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["key"] == "hekma.run_id")
                .unwrap()["value"]["stringValue"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(
        !run_ids.contains(&"run-000".to_string()),
        "the oldest run left the window"
    );
    assert!(
        run_ids.contains(&format!("run-{:03}", MAX_RUNS_PER_INSTANCE as u64)),
        "the newest run stays"
    );
    // The window is NOT a drop: the dropped counter never grew.
    assert_eq!(
        state.dropped(),
        0,
        "evictions are a bounded window, not losses"
    );
}

#[test]
fn a_refreshed_run_survives_while_staler_runs_evict() {
    // Recency is per OBSERVATION, not per first sight: run-000 keeps being
    // observed, so the window evicts run-001 (stale) when the bound hits.
    let mut state = state();
    state.observe(&usage_in_run("agent", "run-000", 10, 10, None));
    for index in 1..=MAX_RUNS_PER_INSTANCE as u64 {
        // run-000 refreshed before every newcomer would evict it.
        state.observe(&usage_in_run("agent", "run-000", 5, 5, None));
        state.observe(&usage_in_run(
            "agent",
            &format!("run-{index:03}"),
            10,
            10,
            None,
        ));
    }
    let body = serde_json::to_value(state.snapshot(2_000)).unwrap();
    let run_ids: Vec<String> = metric(&body, "gen_ai.client.token_usage")["histogram"]
        ["dataPoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            p["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["key"] == "hekma.run_id")
                .unwrap()["value"]["stringValue"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(
        run_ids.contains(&"run-000".to_string()),
        "the refreshed run survives the window"
    );
    assert!(
        !run_ids.contains(&"run-001".to_string()),
        "the stalest run evicted instead"
    );
}
