//! The plain-HTTP OTLP POST client (AD-21): one hyper-util legacy client
//! over an `HttpConnector` — the ~20-line plain-HTTP variant of the metering
//! listener's forward client, rebuilt host-side with types ALREADY in the
//! host's normal dependency closure (hyper / hyper-util / http / bytes —
//! zero new packages in `cargo tree -p hekma-host -e normal`).
//!
//! Wire rules: `POST <endpoint>/v1/metrics`, `Content-Type: application/json`;
//! `200` = delivered; `429`/`503`/`504` and a bounded-timeout are RETRYABLE;
//! everything else (connection refused, any other status, a malformed
//! response) is TERMINAL for the batch — the task layer decides retries, this
//! layer classifies ONE attempt. Every attempt is bounded by an explicit
//! timeout: the export path can never hang supervision (the AD-21 liveness
//! rule).

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

/// The bounded per-attempt POST timeout: a collector that accepts and stalls
/// is classified RETRYABLE when it trips this, never an unbounded hang.
pub const POST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The bounded retry budget for one flush's retryable failures: the FIRST
/// attempt plus at most [`RETRY_ATTEMPTS`] - 1 retries with a small backoff —
/// bounded total work per flush, never a retry storm.
pub const RETRY_ATTEMPTS: u32 = 3;

/// The backoff between retryable attempts (flat and small — a live collector
/// recovers in seconds or the batch is dropped this flush and re-exported
/// with the next cumulative snapshot anyway).
pub const RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(200);

/// The outcome of ONE POST attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostOutcome {
    /// HTTP 200 with an EMPTY `partialSuccess` — the batch was accepted
    /// whole.
    Delivered,
    /// HTTP 200 whose `partialSuccess` is NON-EMPTY (rejected points and/or
    /// an error message, P3b): the batch landed but the collector rejected
    /// part of it — the task must surface the rejection and count the
    /// points, never swallow a partial loss as a clean `200`.
    DeliveredWithRejections {
        /// The `rejectedMetricPoints` count the collector reported.
        rejected_points: u64,
        /// The collector's `errorMessage` (possibly empty when only the
        /// count was reported).
        error_message: String,
    },
    /// A transient failure worth retrying within the bounded budget
    /// (429 / 503 / 504, or the attempt timed out).
    Retryable(String),
    /// A failure that will not change on retry (connection refused, any
    /// other status, a malformed response) — terminal for the batch.
    Terminal(String),
}

/// The configured, ready-to-post exporter client.
pub struct OtlpClient {
    /// The full POST URL (`<endpoint>/v1/metrics`).
    post_url: String,
    /// The startup-resolved headers (secret references already resolved —
    /// the resolved VALUES live here, behind no Debug: this struct's Debug
    /// is manually redacted below).
    headers: Vec<(String, String)>,
    /// The shared legacy client (connection pooling on the host runtime).
    client: Client<HttpConnector, Full<Bytes>>,
}

impl OtlpClient {
    /// Build a client for `endpoint` (validated by [`validate_endpoint`])
    /// with startup-resolved `headers`.
    pub fn new(endpoint: &str, headers: Vec<(String, String)>) -> Result<Self, String> {
        let post_url = build_post_url(endpoint)?;
        Ok(OtlpClient {
            post_url,
            headers,
            client: Client::builder(TokioExecutor::new()).build(HttpConnector::new()),
        })
    }

    /// The full POST URL (exposed for the startup notice and tests).
    pub fn post_url(&self) -> &str {
        &self.post_url
    }

    /// ONE bounded POST attempt of a rendered JSON body. Never panics, never
    /// blocks past [`POST_TIMEOUT`] (the body drain carries its own bound of
    /// the same shape).
    pub async fn post_once(&self, body: String) -> PostOutcome {
        let mut builder = http::Request::builder()
            .method(http::Method::POST)
            .uri(&self.post_url)
            .header(http::header::CONTENT_TYPE, "application/json");
        for (name, value) in &self.headers {
            // `Builder::header` is infallible for well-formed values and
            // returns the builder; an INVALID header value flips the
            // builder's error state, surfaced at `body()` below — so the
            // loop carries the builder straight through.
            builder = builder.header(name.as_str(), value.as_str());
        }
        let request = match builder.body(Full::new(Bytes::from(body))) {
            Ok(request) => request,
            Err(err) => return PostOutcome::Terminal(format!("could not build the POST: {err}")),
        };
        let attempt = self.client.request(request);
        let response = match tokio::time::timeout(POST_TIMEOUT, attempt).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => return PostOutcome::Terminal(format!("the POST failed: {err}")),
            Err(_elapsed) => {
                return PostOutcome::Retryable(
                    "the POST timed out (retryable within the bounded budget)".to_string(),
                )
            }
        };
        let status = response.status();
        // Drain the response body to completion, BOUNDED (P3a): an
        // undrained body pins its pooled connection until idle eviction —
        // the "connection pooling on the host runtime" claim above is only
        // true if every response is consumed. The status already decided
        // the classification, so a drain failure or a trickling body is
        // hygiene-only (the connection is dropped, never the batch).
        let body_bytes = match tokio::time::timeout(
            POST_TIMEOUT,
            BodyExt::collect(response.into_body()),
        )
        .await
        {
            Ok(Ok(collected)) => collected.to_bytes(),
            _ => Bytes::new(),
        };
        match status.as_u16() {
            200 => match parse_partial_success(&body_bytes) {
                // A non-empty partialSuccess is a PARTIAL loss (P3b):
                // delivered, but the rejected points are named and counted
                // by the task — never a silent swallow.
                Some((rejected_points, error_message)) => PostOutcome::DeliveredWithRejections {
                    rejected_points,
                    error_message,
                },
                None => PostOutcome::Delivered,
            },
            429 | 503 | 504 => {
                PostOutcome::Retryable(format!("the collector answered {status} (retryable)"))
            }
            other => PostOutcome::Terminal(format!(
                "the collector answered {other} (terminal for this batch)"
            )),
        }
    }
}

/// Parse a 200 body's `partialSuccess` (P3b): `Some((rejected_points,
/// error_message))` when the collector reports a NON-EMPTY partial success —
/// rejected metric points > 0 and/or an error message. An empty object, an
/// absent field, or a non-JSON body is a clean acceptance (`None`) — the
/// standard's shape, not a guess.
fn parse_partial_success(body: &[u8]) -> Option<(u64, String)> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let partial = value.get("partialSuccess")?;
    let rejected_points = partial
        .get("rejectedMetricPoints")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let error_message = partial
        .get("errorMessage")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    if rejected_points > 0 || !error_message.is_empty() {
        Some((rejected_points, error_message))
    } else {
        None
    }
}

/// One flush's delivery: up to [`RETRY_ATTEMPTS`] bounded attempts with
/// [`RETRY_BACKOFF`] between retryable failures. Returns the LAST outcome
/// when the budget runs out (the task surfaces it and counts the batch).
/// A `200` — clean or with rejections — and a terminal failure both return
/// immediately: only the RETRYABLE class consumes budget.
pub async fn deliver(client: &OtlpClient, body: String) -> PostOutcome {
    let mut outcome = client.post_once(body.clone()).await;
    for _ in 1..RETRY_ATTEMPTS {
        match outcome {
            PostOutcome::Delivered
            | PostOutcome::DeliveredWithRejections { .. }
            | PostOutcome::Terminal(_) => return outcome,
            PostOutcome::Retryable(_) => {
                tokio::time::sleep(RETRY_BACKOFF).await;
                outcome = client.post_once(body.clone()).await;
            }
        }
    }
    outcome
}

/// Strict `--otlp-endpoint` VALIDATION (P3c) — the refusal surface
/// [`OtlpOptions::from_flags`]/[`OtlpOptions::validate`](super::OtlpOptions::validate)
/// consults BEFORE anything is built or dialed, so a malformed endpoint is a
/// rejected INVOCATION (usage class), never a runtime surprise:
///
/// * the URL shape must already pass [`build_post_url`] (http-only scheme,
///   usable authority);
/// * NO query string or fragment (`?`/`#`) — the POST URL is not a template
///   and silently dropping either would POST somewhere the operator did not
///   name;
/// * the authority's port, when present, must be NUMERIC (an unparseable
///   port would die at dial time inside the export loop instead of at flag
///   parse);
/// * the endpoint must NOT already end in `/v1/metrics` — the path is
///   APPENDED by [`build_post_url`], and a doubled
///   `/v1/metrics/v1/metrics` is a collector 404 the operator would have to
///   debug from the export loop.
pub fn validate_endpoint(endpoint: &str) -> Result<(), String> {
    // Shape first (scheme + authority sanity, shared with the builder).
    build_post_url(endpoint).map(|_| ())?;
    if endpoint.contains(['?', '#']) {
        return Err(format!(
            "the OTLP endpoint must not carry a query string or fragment (got \
             {endpoint:?}) — pass the base URL only; the exporter appends the \
             OTLP path itself"
        ));
    }
    let authority = endpoint
        .strip_prefix("http://")
        .unwrap_or(endpoint)
        .split('/')
        .next()
        .unwrap_or("");
    if !port_is_numeric(authority) {
        return Err(format!(
            "the OTLP endpoint's port must be numeric (got {endpoint:?}) — an \
             unparseable port would only fail later, inside the export loop"
        ));
    }
    if endpoint.trim_end_matches('/').ends_with("/v1/metrics") {
        return Err(format!(
            "the OTLP endpoint must be the BASE URL, not the full metrics path \
             (got {endpoint:?}) — the exporter appends /v1/metrics itself"
        ));
    }
    Ok(())
}

/// Whether `authority`'s port segment (when present) is numeric — handles
/// the bracketed IPv6 literal form (`[::1]:4318`) as well as `host:port`.
fn port_is_numeric(authority: &str) -> bool {
    let port = if let Some(rest) = authority.strip_prefix('[') {
        match rest.split_once(']') {
            Some((_host, tail)) => tail.strip_prefix(':'),
            None => None,
        }
    } else {
        authority.rsplit_once(':').map(|(_, port)| port)
    };
    match port {
        None => true,
        Some(port) => !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()),
    }
}

/// Validate an `--otlp-endpoint` value and derive the POST URL. v1 is
/// http-ONLY: `https://` is the documented deferral (TLS handshakes are not
/// provable network-free; the deferral names its trigger), and anything that
/// is not an `http://host[:port][/base]` URL is a typed refusal. The strict
/// flag-time refusals live in [`validate_endpoint`]; this builder only
/// APPENDS the `/v1/metrics` path when the base does not already end in it
/// (P3c — a library caller reaching here directly cannot mint a doubled
/// path).
pub fn build_post_url(endpoint: &str) -> Result<String, String> {
    let rest = match endpoint.strip_prefix("http://") {
        Some(rest) if !rest.is_empty() => rest,
        _ if endpoint.starts_with("https://") => {
            return Err(
                "the OTLP endpoint must be http:// in v1 — https:// is deferred (a \
                 named deferral in docs/telemetry.md; it reopens when an operator \
                 names an https collector as a real need)"
                    .to_string(),
            );
        }
        _ => {
            return Err(format!(
                "the OTLP endpoint must be an http:// URL like http://127.0.0.1:4318 \
                 (got {endpoint:?})"
            ));
        }
    };
    let (authority, base) = match rest.split_once('/') {
        Some((authority, base)) => (authority, Some(base)),
        None => (rest, None),
    };
    if authority.is_empty() || authority.contains(['@', ' ']) {
        return Err(format!(
            "the OTLP endpoint's host:port is not usable (got {authority:?})"
        ));
    }
    let base = match base {
        None => String::new(),
        Some(base) => {
            if base.is_empty() {
                String::new()
            } else {
                format!("/{}", base.trim_end_matches('/'))
            }
        }
    };
    // Append the OTLP path ONLY when the base does not already carry it.
    if base == "/v1/metrics" {
        return Ok(format!("http://{authority}{base}"));
    }
    Ok(format!("http://{authority}{base}/v1/metrics"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_post_url_appends_v1_metrics_to_the_endpoint() {
        assert_eq!(
            build_post_url("http://127.0.0.1:4318").unwrap(),
            "http://127.0.0.1:4318/v1/metrics"
        );
        assert_eq!(
            build_post_url("http://127.0.0.1:4318/").unwrap(),
            "http://127.0.0.1:4318/v1/metrics"
        );
        assert_eq!(
            build_post_url("http://collector.internal:4318/otel").unwrap(),
            "http://collector.internal:4318/otel/v1/metrics"
        );
    }

    #[test]
    fn https_is_a_typed_refusal_naming_the_deferral() {
        let err = build_post_url("https://collector.example.com").unwrap_err();
        assert!(err.contains("deferred"), "{err}");
        assert!(err.contains("http://"), "{err}");
    }

    #[test]
    fn non_http_shapes_are_typed_refusals() {
        assert!(build_post_url("ftp://collector").is_err());
        assert!(build_post_url("collector:4318").is_err());
        assert!(build_post_url("http://").is_err());
        assert!(build_post_url("http:///path").is_err());
    }

    #[test]
    fn the_retry_budget_is_bounded_and_the_classes_are_named() {
        assert_eq!(RETRY_ATTEMPTS, 3);
        assert_eq!(POST_TIMEOUT, std::time::Duration::from_secs(5));
    }

    #[test]
    fn the_post_url_appends_v1_metrics_only_when_absent() {
        // P3c: a base already carrying the path is built VERBATIM (defense
        // in depth for a library caller bypassing validate) — never doubled.
        assert_eq!(
            build_post_url("http://127.0.0.1:4318/v1/metrics").unwrap(),
            "http://127.0.0.1:4318/v1/metrics"
        );
        assert_eq!(
            build_post_url("http://127.0.0.1:4318/v1/metrics/").unwrap(),
            "http://127.0.0.1:4318/v1/metrics"
        );
    }

    #[test]
    fn strict_endpoint_validation_refuses_query_fragments_ports_and_paths() {
        // P3c: the flag-time refusals — each names the offending value.
        let err = validate_endpoint("http://127.0.0.1:4318?token=x").unwrap_err();
        assert!(err.contains("query string"), "{err}");
        let err = validate_endpoint("http://127.0.0.1:4318#fragment").unwrap_err();
        assert!(err.contains("fragment"), "{err}");
        let err = validate_endpoint("http://127.0.0.1:otlp").unwrap_err();
        assert!(err.contains("numeric"), "{err}");
        let err = validate_endpoint("http://127.0.0.1:4318/v1/metrics").unwrap_err();
        assert!(err.contains("BASE URL"), "{err}");
        // The healthy shapes pass.
        assert!(validate_endpoint("http://127.0.0.1:4318").is_ok());
        assert!(validate_endpoint("http://collector.internal:4318/otel").is_ok());
        assert!(validate_endpoint("http://[::1]:4318").is_ok());
    }

    #[test]
    fn partial_success_parsing_names_rejected_points_and_messages() {
        // P3b: an empty/absent partialSuccess is a clean acceptance.
        assert_eq!(parse_partial_success(b""), None);
        assert_eq!(parse_partial_success(b"{}"), None);
        assert_eq!(parse_partial_success(br#"{"partialSuccess":{}}"#), None);
        // Rejected points, with and without a message.
        assert_eq!(
            parse_partial_success(br#"{"partialSuccess":{"rejectedMetricPoints":7}}"#),
            Some((7, "".to_string()))
        );
        assert_eq!(
            parse_partial_success(
                br#"{"partialSuccess":{"rejectedMetricPoints":3,"errorMessage":"unknown metric"}}"#
            ),
            Some((3, "unknown metric".to_string()))
        );
        // A message alone (count absent) is still a surfaced rejection.
        assert_eq!(
            parse_partial_success(br#"{"partialSuccess":{"errorMessage":"quota"}}"#),
            Some((0, "quota".to_string()))
        );
    }
}
