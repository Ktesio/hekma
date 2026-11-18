use super::*;
use serde_json::json;

fn kv(key: &str, value: &str) -> serde_json::Value {
    json!({"key": key, "value": {"stringValue": value}})
}

fn usage_point(token_type: &str, value: serde_json::Value) -> serde_json::Value {
    json!({
        "attributes": [kv("gen_ai.token.type", token_type), kv("gen_ai.request.model", "gpt-demo")],
        "asInt": value,
    })
}

fn resource(
    attributes: Vec<serde_json::Value>,
    metrics: Vec<serde_json::Value>,
) -> serde_json::Value {
    json!({
        "resource": {"attributes": attributes},
        "scopeMetrics": [{"scope": {"name": "SENTINEL-scope"}, "metrics": metrics}],
    })
}

fn token_usage(
    points: Vec<serde_json::Value>,
    temporality: serde_json::Value,
) -> serde_json::Value {
    json!({
        "name": "gen_ai.client.token_usage",
        "unit": "SENTINEL-unit",
        "sum": {"dataPoints": points, "aggregationTemporality": temporality, "isMonotonic": true},
    })
}

fn read(body: serde_json::Value) -> IntakeBatch {
    read_batch(body.to_string().as_bytes()).unwrap()
}

#[test]
fn a_batch_keeps_the_identity_labels_and_cumulative_token_sums_only() {
    let batch = read(json!({"resourceMetrics": [resource(
        vec![
            kv("hekma.run.external_id", "job-7"),
            kv("hekma.label.team", "core"),
            kv("service.name", "SENTINEL-service"),
        ],
        vec![
            token_usage(
                vec![usage_point("input", json!("1200")), usage_point("cached", json!(100))],
                json!(2),
            ),
            json!({"name": "SENTINEL-metric", "sum": {"dataPoints": [{"asInt": "1"}], "aggregationTemporality": 2}}),
        ],
    )]}));
    assert_eq!(batch.identities.len(), 1);
    let identity = &batch.identities[0];
    assert_eq!(identity.external_id, "job-7");
    assert_eq!(
        identity.labels,
        vec![("team".to_string(), "core".to_string())]
    );
    assert_eq!(identity.points.len(), 2);
    assert_eq!(identity.points[0].token_type, TelemetryTokenType::Input);
    assert_eq!(identity.points[0].value, 1200);
    assert_eq!(identity.points[1].value, 100);
    assert_eq!(identity.points[0].model.as_deref(), Some("gpt-demo"));
    assert_eq!(batch.skipped.get(&SkipReason::Shape), Some(&1));
    // Nothing the receiver does not consume survives the read.
    let kept = format!("{batch:?}");
    assert!(!kept.contains("SENTINEL"), "{kept}");
}

#[test]
fn every_unreadable_point_is_skipped_and_counted_by_reason() {
    let many_labels: Vec<serde_json::Value> =
        std::iter::once(kv("hekma.run.external_id", "labelled"))
            .chain((0..17).map(|i| kv(&format!("hekma.label.k{i}"), "v")))
            .collect();
    let cumulative = json!("AGGREGATION_TEMPORALITY_CUMULATIVE");
    let batch = read(json!({"resourceMetrics": [
        // No identity: its points are skipped.
        resource(vec![], vec![token_usage(vec![usage_point("input", json!("5"))], json!(2))]),
        // An id with a space, and one over 128 characters.
        resource(vec![kv("hekma.run.external_id", "has space")],
                 vec![token_usage(vec![usage_point("input", json!("5"))], json!(2))]),
        resource(vec![kv("hekma.run.external_id", &"x".repeat(129))],
                 vec![token_usage(vec![usage_point("input", json!("5"))], json!(2))]),
        // Seventeen labels: past the bound.
        resource(many_labels, vec![token_usage(vec![usage_point("input", json!("5"))], cumulative.clone())]),
        // A delta sum, a gauge, a point without a token type, an unknown type.
        resource(vec![kv("hekma.run.external_id", "shapes")], vec![
            token_usage(vec![usage_point("input", json!("5"))], json!(1)),
            json!({"name": "gen_ai.client.token_usage", "gauge": {"dataPoints": [{}, {}]}}),
            token_usage(vec![json!({"asInt": "3"}), usage_point("reasoning", json!("3")),
                             usage_point("output", json!(7))], cumulative.clone()),
        ]),
    ]}));
    assert_eq!(batch.skipped.get(&SkipReason::Identity), Some(&3));
    assert_eq!(batch.skipped.get(&SkipReason::Bounds), Some(&1));
    assert_eq!(batch.skipped.get(&SkipReason::Shape), Some(&5));
    assert_eq!(batch.identities.len(), 1);
    assert_eq!(batch.identities[0].external_id, "shapes");
    assert_eq!(batch.identities[0].points.len(), 1);
}

#[test]
fn the_batch_bounds_skip_past_eight_resources_and_sixty_four_points() {
    let resources: Vec<serde_json::Value> = (0..9)
        .map(|i| {
            resource(
                vec![kv("hekma.run.external_id", &format!("r{i}"))],
                vec![token_usage(
                    vec![usage_point("input", json!("1"))],
                    json!(2),
                )],
            )
        })
        .collect();
    let batch = read(json!({ "resourceMetrics": resources }));
    assert_eq!(batch.identities.len(), 8);
    assert_eq!(batch.skipped.get(&SkipReason::Bounds), Some(&1));
    let points: Vec<serde_json::Value> =
        (0..70).map(|_| usage_point("input", json!("1"))).collect();
    let batch = read(json!({"resourceMetrics": [resource(
        vec![kv("hekma.run.external_id", "many")],
        vec![token_usage(points, json!(2))],
    )]}));
    assert_eq!(batch.identities[0].points.len(), 64);
    assert_eq!(batch.skipped.get(&SkipReason::Bounds), Some(&6));
    // One identity across two resources merges its points.
    let twice = |value: &str| {
        resource(
            vec![kv("hekma.run.external_id", "same")],
            vec![token_usage(
                vec![usage_point("input", json!(value))],
                json!(2),
            )],
        )
    };
    let batch = read(json!({"resourceMetrics": [twice("1"), twice("2")]}));
    assert_eq!(batch.identities.len(), 1);
    assert_eq!(batch.identities[0].points.len(), 2);
}

#[test]
fn malformed_bodies_and_content_types_are_told_apart() {
    assert!(read_batch(b"not json").is_err());
    assert!(read_batch(b"{\"resourceMetrics\": 5}").is_err());
    assert_eq!(read_batch(b"{}").unwrap(), IntakeBatch::default());
    let headers = |value: &str| {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(value).unwrap());
        headers
    };
    assert!(is_json(&headers("application/json")));
    assert!(is_json(&headers("Application/JSON; charset=utf-8")));
    assert!(!is_json(&headers("application/x-protobuf")));
    assert!(!is_json(&headers("application/grpc")));
    assert!(!is_json(&HeaderMap::new()));
    assert_eq!(as_u64(Some(&json!("12"))), Some(12));
    assert_eq!(as_u64(Some(&json!(12))), Some(12));
    assert_eq!(as_u64(Some(&json!("x"))), None);
    assert_eq!(as_u64(Some(&json!(true))), None);
    assert!(!cumulative(None));
}

#[test]
fn partial_success_carries_fixed_messages_only() {
    let mut skipped = BTreeMap::new();
    assert_eq!(partial_success(&skipped).status(), StatusCode::OK);
    skipped.insert(SkipReason::WireRun, 2);
    skipped.insert(SkipReason::Shape, 1);
    assert_eq!(partial_success(&skipped).status(), StatusCode::OK);
    for reason in [
        SkipReason::Identity,
        SkipReason::Shape,
        SkipReason::Bounds,
        SkipReason::WireRun,
    ] {
        assert!(!reason.message().is_empty());
    }
}
