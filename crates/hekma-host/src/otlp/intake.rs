//! Inbound OTLP (story 23-3, spine AD-28): `POST /v1/metrics` on the
//! Control API listener, OTLP/HTTP+JSON only, under a binding's announce
//! credential.
//!
//! The route is a thin adapter: it reads exactly three things from a batch
//! and hands them to [`hekma_engine::Engine::ingest_telemetry`] —
//!
//! * the resource attribute `hekma.run.external_id` (the Run's identity,
//!   1 to 128 visible ASCII characters);
//! * `hekma.label.*` resource attributes (Caller labels, at most 16);
//! * cumulative `gen_ai.client.token_usage` sum points carrying
//!   `gen_ai.token.type` = `input`, `output` or `cached` (and, for pricing
//!   only, `gen_ai.request.model`).
//!
//! Nothing else is read and nothing else is stored. Every other point is
//! skipped, counted, and named in the response's `partialSuccess` with a
//! fixed message — never a client string. These structs mirror the
//! opentelemetry-proto JSON mapping for READING; the exporter's
//! Serialize-only `payload.rs` (the frozen `otlp-metrics v1`) is untouched.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use hekma_engine::{
    RunError, TelemetryOutcome, TelemetryPoint, TelemetryReport, TelemetryTokenType,
};
use serde::Deserialize;

use crate::control::auth::Authority;
use crate::control::error::{ApiError, ApiErrorKind};
use crate::control::ApiState;

/// The largest body the receiver reads (`413` past it, nothing parsed).
pub(crate) const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
/// The most `resourceMetrics` one batch carries (the rest are skipped).
pub(crate) const MAX_RESOURCES: usize = 8;
/// The most points one metric family carries per batch.
pub(crate) const MAX_POINTS_PER_FAMILY: usize = 64;
/// The most `hekma.label.*` attributes one resource carries.
pub(crate) const MAX_LABELS: usize = 16;

/// The identity attribute.
pub(crate) const EXTERNAL_ID_ATTRIBUTE: &str = "hekma.run.external_id";
/// The Caller-label attribute prefix.
pub(crate) const LABEL_PREFIX: &str = "hekma.label.";
/// The one metric read.
pub(crate) const TOKEN_USAGE_METRIC: &str = "gen_ai.client.token_usage";

/// Why points were skipped (each with a fixed message).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SkipReason {
    /// No valid `hekma.run.external_id` on the resource.
    Identity,
    /// Not a cumulative `gen_ai.client.token_usage` sum point with a known
    /// `gen_ai.token.type`.
    Shape,
    /// Past the batch bounds.
    Bounds,
    /// The Run's calls pass through the metering proxy.
    WireRun,
}

impl SkipReason {
    fn message(self) -> &'static str {
        match self {
            SkipReason::Identity => {
                "points without a valid hekma.run.external_id resource attribute were skipped"
            }
            SkipReason::Shape => {
                "only cumulative gen_ai.client.token_usage sums with gen_ai.token.type input, \
                 output or cached are read; other points were skipped"
            }
            SkipReason::Bounds => {
                "points past the batch bounds (8 resources, 64 points per metric, 16 labels) \
                 were skipped"
            }
            SkipReason::WireRun => {
                "this Run's calls pass through the metering proxy, whose per-call metering is \
                 its usage; its telemetry points were skipped"
            }
        }
    }
}

/// One identity's share of a batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IntakeIdentity {
    pub external_id: String,
    pub labels: Vec<(String, String)>,
    pub points: Vec<TelemetryPoint>,
}

/// What [`read_batch`] kept, and what it skipped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct IntakeBatch {
    pub identities: Vec<IntakeIdentity>,
    pub skipped: BTreeMap<SkipReason, u64>,
}

impl IntakeBatch {
    fn skip(&mut self, reason: SkipReason, count: usize) {
        if count > 0 {
            *self.skipped.entry(reason).or_default() += count as u64;
        }
    }
}

// The opentelemetry-proto JSON mapping, read side. Unknown fields are
// ignored; nothing here is ever logged.

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ExportRequest {
    #[serde(default)]
    resource_metrics: Vec<ResourceMetrics>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ResourceMetrics {
    #[serde(default)]
    resource: Option<Resource>,
    #[serde(default)]
    scope_metrics: Vec<ScopeMetrics>,
}

#[derive(Deserialize, Default)]
struct Resource {
    #[serde(default)]
    attributes: Vec<KeyValue>,
}

#[derive(Deserialize)]
struct KeyValue {
    key: String,
    #[serde(default)]
    value: Option<AnyValue>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AnyValue {
    #[serde(default)]
    string_value: Option<String>,
}

#[derive(Deserialize, Default)]
struct ScopeMetrics {
    #[serde(default)]
    metrics: Vec<Metric>,
}

#[derive(Deserialize, Default)]
struct Metric {
    #[serde(default)]
    name: String,
    #[serde(default)]
    sum: Option<Sum>,
    #[serde(default)]
    gauge: Option<PointCount>,
    #[serde(default)]
    histogram: Option<PointCount>,
    #[serde(default, rename = "exponentialHistogram")]
    exponential_histogram: Option<PointCount>,
    #[serde(default)]
    summary: Option<PointCount>,
}

/// A family this receiver does not read: only its point count matters.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PointCount {
    #[serde(default)]
    data_points: Vec<serde::de::IgnoredAny>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Sum {
    #[serde(default)]
    data_points: Vec<NumberDataPoint>,
    #[serde(default)]
    aggregation_temporality: Option<serde_json::Value>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct NumberDataPoint {
    #[serde(default)]
    attributes: Vec<KeyValue>,
    #[serde(default)]
    as_int: Option<serde_json::Value>,
}

/// The cumulative temporality, in either JSON form.
fn cumulative(temporality: Option<&serde_json::Value>) -> bool {
    match temporality {
        Some(serde_json::Value::Number(n)) => n.as_u64() == Some(2),
        Some(serde_json::Value::String(s)) => s == "AGGREGATION_TEMPORALITY_CUMULATIVE",
        _ => false,
    }
}

/// A 64-bit integer in either JSON form (the mapping writes int64 as a
/// decimal string; some exporters write a number).
fn as_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    match value? {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn string_attribute<'a>(attributes: &'a [KeyValue], key: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|value| value.string_value.as_deref())
}

fn points_of(metric: &Metric) -> usize {
    metric.sum.as_ref().map_or(0, |sum| sum.data_points.len())
        + [
            &metric.gauge,
            &metric.histogram,
            &metric.exponential_histogram,
            &metric.summary,
        ]
        .into_iter()
        .flatten()
        .map(|family| family.data_points.len())
        .sum::<usize>()
}

/// Read a batch (pure): keep only the three documented inputs, count the
/// rest. `Err` when the body is not an OTLP/HTTP+JSON metrics request.
pub(crate) fn read_batch(body: &[u8]) -> Result<IntakeBatch, ()> {
    let request: ExportRequest = serde_json::from_slice(body).map_err(|_| ())?;
    let mut batch = IntakeBatch::default();
    let mut token_points = 0usize;
    for (index, resource) in request.resource_metrics.iter().enumerate() {
        let metrics = resource.scope_metrics.iter().flat_map(|s| s.metrics.iter());
        if index >= MAX_RESOURCES {
            batch.skip(SkipReason::Bounds, metrics.map(points_of).sum());
            continue;
        }
        let attributes = resource
            .resource
            .as_ref()
            .map(|r| r.attributes.as_slice())
            .unwrap_or_default();
        let id = string_attribute(attributes, EXTERNAL_ID_ATTRIBUTE)
            .map(str::trim)
            .filter(|id| valid_external_id(id));
        let labels: Vec<(String, String)> = attributes
            .iter()
            .filter_map(|kv| {
                let key = kv.key.strip_prefix(LABEL_PREFIX)?;
                let value = kv.value.as_ref()?.string_value.as_deref()?;
                Some((key.to_string(), value.to_string()))
            })
            .collect();
        let labels_ok = labels.len() <= MAX_LABELS
            && labels.iter().all(|(k, v)| {
                hekma_engine::validate_label_key(k).is_ok()
                    && hekma_engine::validate_label_value(v).is_ok()
            });
        let mut points = Vec::new();
        for metric in metrics {
            let total = points_of(metric);
            let readable = metric.name == TOKEN_USAGE_METRIC
                && metric
                    .sum
                    .as_ref()
                    .is_some_and(|sum| cumulative(sum.aggregation_temporality.as_ref()));
            if !readable {
                batch.skip(SkipReason::Shape, total);
                continue;
            }
            for point in &metric.sum.as_ref().expect("readable").data_points {
                let token_type = string_attribute(&point.attributes, "gen_ai.token.type")
                    .and_then(TelemetryTokenType::from_wire);
                let value = as_u64(point.as_int.as_ref());
                let (Some(token_type), Some(value)) = (token_type, value) else {
                    batch.skip(SkipReason::Shape, 1);
                    continue;
                };
                if token_points >= MAX_POINTS_PER_FAMILY {
                    batch.skip(SkipReason::Bounds, 1);
                    continue;
                }
                token_points += 1;
                points.push(TelemetryPoint {
                    token_type,
                    value,
                    model: string_attribute(&point.attributes, "gen_ai.request.model")
                        .map(str::to_string),
                });
            }
        }
        let Some(id) = id else {
            batch.skip(SkipReason::Identity, points.len());
            continue;
        };
        if !labels_ok {
            batch.skip(SkipReason::Bounds, points.len());
            continue;
        }
        if points.is_empty() {
            continue;
        }
        match batch.identities.iter_mut().find(|i| i.external_id == id) {
            Some(existing) => existing.points.extend(points),
            None => batch.identities.push(IntakeIdentity {
                external_id: id.to_string(),
                labels,
                points,
            }),
        }
    }
    Ok(batch)
}

/// 1 to 128 visible ASCII characters (the hint rule).
fn valid_external_id(raw: &str) -> bool {
    !raw.is_empty() && raw.len() <= 128 && raw.bytes().all(|b| b.is_ascii_graphic())
}

/// Whether the request's content type is JSON (`application/json`, with an
/// optional parameter such as `charset=utf-8`).
fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or("").trim())
        .is_some_and(|mime| mime.eq_ignore_ascii_case("application/json"))
}

/// The `partialSuccess` answer (fixed messages only).
fn partial_success(skipped: &BTreeMap<SkipReason, u64>) -> Response {
    let rejected: u64 = skipped.values().sum();
    if rejected == 0 {
        return StatusCode::OK.into_response();
    }
    let message = skipped
        .keys()
        .map(|reason| reason.message())
        .collect::<Vec<_>>()
        .join("; ");
    let body = serde_json::json!({
        "partialSuccess": {
            "rejectedDataPoints": rejected.to_string(),
            "errorMessage": message,
        }
    });
    (StatusCode::OK, axum::Json(body)).into_response()
}

/// `POST /v1/metrics` (story 23-3) — see the module docs. Only an announce
/// credential reaches this handler (the matrix).
pub(crate) async fn metrics_intake(
    State(state): State<ApiState>,
    Extension(authority): Extension<Authority>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Authority::Announcer(credential) = &authority else {
        return ApiError::forbidden().into_response();
    };
    if !is_json(&headers) {
        return ApiError::new(
            ApiErrorKind::InvalidBody,
            "this receiver accepts OTLP/HTTP+JSON only (content-type application/json); send \
             protobuf or gRPC through an OpenTelemetry Collector whose otlphttp exporter posts \
             JSON (see the telemetry docs)",
        )
        .with_status(StatusCode::UNSUPPORTED_MEDIA_TYPE)
        .into_response();
    }
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return ApiError::new(
            ApiErrorKind::InvalidBody,
            "the metrics body is over the 4 MiB limit, so nothing was read",
        )
        .with_status(StatusCode::PAYLOAD_TOO_LARGE)
        .into_response();
    };
    let Ok(mut batch) = read_batch(&bytes) else {
        return ApiError::new(
            ApiErrorKind::InvalidBody,
            "the body is not an OTLP/HTTP+JSON metrics export request",
        )
        .into_response();
    };
    let mut rebased = 0u64;
    let mut skipped_labels = 0u64;
    let identities = std::mem::take(&mut batch.identities);
    for identity in identities {
        let report = TelemetryReport {
            credential: credential.clone(),
            external_id: identity.external_id,
            labels: identity.labels,
            points: identity.points,
        };
        match state.engine.ingest_telemetry(report).await {
            Ok(TelemetryOutcome::Committed {
                rebased: reset,
                skipped_labels: labels,
                ..
            }) => {
                rebased += u64::from(reset);
                skipped_labels += labels as u64;
            }
            Ok(TelemetryOutcome::NoDelta {
                skipped_labels: labels,
                ..
            }) => skipped_labels += labels as u64,
            Ok(TelemetryOutcome::SkippedWireRun { points, .. }) => {
                batch.skip(SkipReason::WireRun, points);
            }
            Err(RunError::Store(hekma_engine::ports::StoreError::Busy { .. })) => {
                let mut response = ApiError::new(
                    ApiErrorKind::Unavailable,
                    "the state store did not take the batch in time; retry it (a retry never \
                     counts the same tokens twice)",
                )
                .into_response();
                response
                    .headers_mut()
                    .insert("x-should-retry", HeaderValue::from_static("true"));
                return response;
            }
            Err(err) => return ApiError::for_authority(&authority, err).into_response(),
        }
    }
    state
        .auth_diagnostics
        .intake(batch.skipped.values().sum(), skipped_labels, rebased);
    partial_success(&batch.skipped)
}

#[cfg(test)]
mod tests;
