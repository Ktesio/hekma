//! The relay pieces the shared Metering Proxy forwards with (moved from the
//! per-instance listener of story 3-4, story 23-1): the TLS-capable forward
//! client, the body cap, the hop-by-hop strip, the `stream_options`
//! injection and the request-body drain.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::HeaderName;
use hyper::HeaderMap;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

/// The outbound forward client type (story 12-3): hyper-util's legacy client
/// over a `hyper-rustls` HTTPS connector. The connector routes by scheme —
/// `http://` upstreams dial plain TCP exactly as before; `https://` upstreams
/// handshake TLS through the VENDORED rustls+ring stack (no system TLS). One
/// client type for both schemes, so the relay logic never branches on TLS.
pub(crate) type ForwardClient = Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Full<Bytes>,
>;

/// The PRODUCTION TLS client configuration (story 12-3): the vendored
/// `webpki-roots` public CA store — NO system trust store is read (the
/// no-system-dependency philosophy; identical roots on all three OSes, and the
/// same vendored-roots choice `kt`→`ureq` already ships). Named + injectable so
/// the TLS test can substitute a config trusting a self-signed root (see
/// the story-12-3 `start_with_tls_config` test seam).
/// The vendored PRODUCTION root store (webpki-roots — the no-system-TLS
/// philosophy, matching ureq's choice). Extracted from
/// [`default_tls_config`] (review round 2) so a unit test can observe the
/// store DIRECTLY: an accidentally-empty root store would compile, pass every
/// in-suite test (whose https legs trust a custom self-signed root instead),
/// and still fail every PRODUCTION https forward at handshake — the one
/// failure mode the injection/seam tests structurally cannot catch.
pub(crate) fn default_root_store() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

/// The default client config plus one extra trusted root (DER): the custody
/// tests' TLS fake upstream.
#[cfg(test)]
pub(crate) fn tls_config_with_root(der: &[u8]) -> Arc<rustls::ClientConfig> {
    let mut roots = default_root_store();
    roots
        .add(rustls::pki_types::CertificateDer::from(der.to_vec()))
        .expect("a valid test root");
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

pub(crate) fn default_tls_config() -> Arc<rustls::ClientConfig> {
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(default_root_store())
            .with_no_client_auth(),
    )
}

/// The MAXIMUM size (bytes) of a single request OR response body this transparent
/// proxy will BUFFER before failing the relay cleanly (story 3-4 hardening).
///
/// The `forward()` relay must `collect()` each body into memory to BOTH relay it
/// faithfully AND skim the `usage` object — an unbounded `collect()` on a runaway
/// or malicious multi-GB body would exhaust engine memory (an OOM DoS). We wrap
/// both directions in [`Limited`] at this cap: a body that exceeds it fails as a
/// clean, traffic-free `502` (never a partial/corrupt relay, never an unbounded
/// alloc, never a panic).
///
/// The value is deliberately GENEROUS — 64 MiB — so it bounds a pathological body
/// while never truncating a REAL LLM completion (even a very large non-streaming
/// JSON completion with long tool-call arguments is orders of magnitude under this;
/// typical completions are well under 1 MiB). It is a named module constant, tuned
/// here in one place rather than a magic number inline at each `collect()`.
pub(crate) const MAX_RELAY_BODY_BYTES: usize = 64 * 1024 * 1024;

/// The RFC 7230 §6.1 HOP-BY-HOP headers — meaningful only for a single transport
/// hop, and MUST NOT be forwarded by a proxy. We STRIP these from the buffered
/// response (and defensively the request) before re-attaching the body: the relay
/// rebuilds a FIXED-LENGTH `Full<Bytes>` body, so hyper's server frames it itself.
/// Carrying a `chunked` upstream's `Transfer-Encoding` (or a now-wrong
/// `Content-Length`) onto that known-length body would produce a conflicting/lossy
/// frame the agent's client could mis-read. Stripping them lets hyper set the
/// correct `Content-Length` for the exact bytes we relay. Matched case-insensitively
/// (`HeaderName` compares ASCII-case-insensitively).
pub(crate) const HOP_BY_HOP_HEADERS: &[HeaderName] = &[
    hyper::header::CONNECTION,
    hyper::header::TE,
    hyper::header::TRANSFER_ENCODING,
    hyper::header::TRAILER,
    hyper::header::UPGRADE,
    hyper::header::PROXY_AUTHENTICATE,
    hyper::header::PROXY_AUTHORIZATION,
    hyper::header::CONTENT_LENGTH,
];

/// Read a REFUSED request's body to its end, discarding every frame, so a
/// keep-alive client that is still uploading receives the refusal instead of a
/// connection reset (which SDKs retry as a transport error). Nothing is kept,
/// and the read stops at the relay cap or the first transport error.
pub(crate) async fn drain_request_body(mut body: Incoming) {
    let mut seen: usize = 0;
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            return;
        };
        if let Some(data) = frame.data_ref() {
            seen = seen.saturating_add(data.len());
            if seen > MAX_RELAY_BODY_BYTES {
                return;
            }
        }
    }
}

/// Whether the response headers declare a Server-Sent-Events body — the
/// content-type route that selects the terminal-frame SSE scanner (story 12-2).
/// Matched on the media type, case-insensitively; an absent/unparseable
/// content-type is "not SSE" (the JSON completion parse then applies, which is
/// the safe default: it skips cleanly on anything that is not one JSON object).
pub(crate) fn is_sse(headers: &HeaderMap) -> bool {
    headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().contains("text/event-stream"))
        .unwrap_or(false)
}

/// The story-12-2 REQUEST INJECTION: on a forwarded STREAMING completion request,
/// add `"stream_options": {"include_usage": true}` so the provider emits the
/// terminal SSE usage frame (which [`super::parse::parse_openai_sse_usage`] then
/// parses). Best-effort and deliberately narrow — the injection happens ONLY when:
/// * the method is `POST` (the OpenAI completion shape), and
/// * the request PATH ends `/chat/completions` (story 12-2 AMENDMENT, review
///   loop 1: the SAME surface whose response shape
///   [`super::parse::parse_openai_usage`]/[`super::parse::parse_openai_sse_usage`]
///   understand — any other streaming endpoint (embeddings, completions,
///   provider-specific routes) rejects `stream_options` with a 400, so the
///   injection there would BREAK the call it meters), and
/// * the request declares a JSON content-type, and
/// * the body parses as a JSON OBJECT that already sets `"stream": true`
///   (never fabricating a stream on a non-streaming request), and
/// * the body carries NO `stream_options` key (the agent's own choice wins —
///   if it explicitly asked for NO usage frame, that is respected).
///
/// "Outside the path gate" means usage is not REQUESTED there — nothing is
/// injected — but it does NOT mean unmeterable: the RESPONSE parse routes on
/// content-type, not path, so any usage the provider sends unprompted on a
/// metered response is still parsed and recorded (see review round 2's
/// wording fix). Any other shape — a malformed body, a non-object, a GET, any
/// other path — leaves the body UNMODIFIED (the relay stays faithful; the
/// metering miss is the accepted, lower-bound-honest cost). The
/// re-serialization is the house lenient-parse round-trip: field ORDER may
/// change on the wire (providers do not care), the CONTENT does not. Mutates
/// `req_bytes` in place when the injection lands; returns nothing (the
/// injection is silent — the observable fact is documented upstream-visible,
/// not logged traffic).
pub(crate) fn inject_include_usage(
    method: &hyper::Method,
    path: &str,
    headers: &HeaderMap,
    req_bytes: &mut Bytes,
) {
    if method != hyper::Method::POST {
        return;
    }
    // Story 12-2 AMENDMENT (review loop 1): the PATH gate. Mirrors the parse
    // scope — only `/chat/completions` responses are read for usage, so only
    // those requests are modified. Exact-suffix match on the URI path (the
    // query string is not part of `path()`); no trailing-slash leniency (the
    // OpenAI surface is `/…/chat/completions`, and inventing extra accepted
    // shapes would widen the modified-request surface).
    if !path.ends_with("/chat/completions") {
        return;
    }
    let is_json = headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().contains("application/json"))
        .unwrap_or(false);
    if !is_json {
        return;
    }
    // Lenient parse (the same house style the response parse uses): a body that
    // does not parse as a JSON object is forwarded unmodified.
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(req_bytes) else {
        return;
    };
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    // Gate on the agent's OWN streaming intent.
    if !obj
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return;
    }
    // Respect an existing stream_options — never overwrite the agent's choice.
    if obj.contains_key("stream_options") {
        return;
    }
    obj.insert(
        "stream_options".to_string(),
        serde_json::json!({ "include_usage": true }),
    );
    // A serialization failure (not reachable for a Value) leaves the original
    // body in place — best-effort, never a relay failure.
    if let Ok(serialized) = serde_json::to_vec(&value) {
        *req_bytes = Bytes::from(serialized);
    }
}

/// Remove every hop-by-hop header from `headers` in place. Also honors the
/// `Connection: <token>` list per RFC 7230 §6.1: any header NAMED in a `Connection`
/// header value is itself connection-specific and stripped too, before `Connection`
/// itself is removed. Leaves all end-to-end headers (`Content-Type`, etc.) intact.
pub(crate) fn strip_hop_by_hop(headers: &mut HeaderMap) {
    // First collect any header names listed in `Connection: a, b` — those are
    // connection-specific for THIS hop and must not be forwarded either.
    let connection_listed: Vec<HeaderName> = headers
        .get_all(hyper::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|tok| HeaderName::try_from(tok.trim()).ok())
        .collect();
    for name in &connection_listed {
        headers.remove(name);
    }
    // Then the fixed RFC hop-by-hop set (incl. Connection, Transfer-Encoding,
    // Content-Length — hyper reframes the known-length Full body itself).
    for name in HOP_BY_HOP_HEADERS {
        headers.remove(name);
    }
}

/// The shared proxy's forward client: a 10 s upstream connect timeout
/// (story 23-1) and no redirect following (the client never follows one —
/// an upstream `3xx` is relayed).
pub(crate) fn proxy_client(tls: Arc<rustls::ClientConfig>) -> ForwardClient {
    let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(std::time::Duration::from_secs(10)));
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config((*tls).clone())
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Client::builder(TokioExecutor::new()).build(https)
}
