//! `proxy-wire v1` (story 23-1, additive from 19-2's slice): every response
//! the shared Metering Proxy makes itself, every header name it reads or
//! writes, and the strip rule — one module, one table.
//!
//! Every refusal is `{"type":"error","error":{"type":"hekma_…","code":"…",
//! "message":"…","param":null,"proxy_wire":"v1"}}` with `content-type:
//! application/json` and `x-should-retry`. No body carries a limit, a total,
//! an instance name, a token, a token id or any request or response content:
//! every interpolated value is a fixed engine string.

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{HeaderMap, Method, Response, StatusCode};

use crate::domain::{BreachDimension, BreachScope, FenceRefusal, LatchKey, PROXY_WIRE_VERSION};

/// The Run Key identification header (request).
pub(crate) const RUN_KEY_HEADER: &str = "x-hekma-run-key";
/// The Run hint header (request).
pub(crate) const RUN_HINT_HEADER: &str = "x-hekma-run";
/// The Caller labels header (request; outbound identity header too).
pub(crate) const LABELS_HEADER: &str = "x-hekma-labels";
/// The outbound identity header naming the Run.
pub(crate) const RUN_ID_HEADER: &str = "x-hekma-run-id";
/// The announce header (request, story 23-2): claim this external id.
pub(crate) const ANNOUNCE_HEADER: &str = "x-hekma-announce";
/// The response header carrying a first announce's Run Key (story 23-2;
/// the request header of the same name presents one).
pub(crate) const RUN_KEY_RESPONSE_HEADER: &str = "x-hekma-run-key";
/// The response header carrying a first announce's refresh credential.
pub(crate) const REFRESH_RESPONSE_HEADER: &str = "x-hekma-refresh";
/// The response header naming a Run's latched breaches.
pub(crate) const BREACH_HEADER: &str = "x-hekma-breach";
/// Every request header with this prefix is stripped before forwarding.
pub(crate) const STRIP_PREFIX: &str = "x-hekma-";
/// The fixed mask that replaces every exact occurrence of the custodied
/// key in a relayed response (story 22-2): providers can echo a presented
/// key in an error body, and the response the agent reads must not carry
/// the real key back into everything the agent prints or writes.
pub(crate) const REDACTION_MASK: &str = "hekma_redacted_key";

/// One `proxy-wire v1` refusal row (the table in the module docs and in
/// `docs/metering-proxy.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WireError {
    CredentialInUrl,
    CustodyUnavailable,
    BadTarget,
    BadHost,
    PathNotAllowed,
    UnscentedCall,
    UnknownCredential,
    CredentialRevoked,
    RunOutOfScope,
    NoRoute,
    BodyTooLarge,
    ProxyBusy,
    LedgerUnavailable,
    RunUnavailable,
    UpstreamError,
    /// Story 23-4: an Operator rejected the Run's calls.
    RunReject,
    /// Story 23-4: the Run's hold queue is full.
    HoldFull,
    /// Story 23-4: the host stopped while the call was held.
    HoldInterrupted,
}

impl WireError {
    /// Every row (the table tests walk it).
    #[cfg(test)]
    pub(crate) const ALL: [WireError; 18] = [
        WireError::CredentialInUrl,
        WireError::CustodyUnavailable,
        WireError::BadTarget,
        WireError::BadHost,
        WireError::PathNotAllowed,
        WireError::UnscentedCall,
        WireError::UnknownCredential,
        WireError::CredentialRevoked,
        WireError::RunOutOfScope,
        WireError::NoRoute,
        WireError::BodyTooLarge,
        WireError::ProxyBusy,
        WireError::LedgerUnavailable,
        WireError::RunUnavailable,
        WireError::UpstreamError,
        WireError::RunReject,
        WireError::HoldFull,
        WireError::HoldInterrupted,
    ];

    /// (status, `error.type`, `error.code`, message, retryable). `None`
    /// retryable = no `x-should-retry` header (the SDK default retry).
    pub(crate) fn row(self) -> (u16, &'static str, &'static str, &'static str, Option<bool>) {
        match self {
            WireError::CredentialInUrl => (
                400,
                "hekma_credential_in_url",
                "credential_in_url",
                "Credentials never go in a URL; send them in a header. This request was not \
                 forwarded.",
                Some(false),
            ),
            WireError::CustodyUnavailable => (
                400,
                "hekma_custody_unavailable",
                "custody_unavailable",
                "This Run Key identifies the Run, but Hekma does not hold a provider key for it \
                 yet. Send your provider key in Authorization and the Run Key in \
                 x-hekma-run-key.",
                Some(false),
            ),
            WireError::BadTarget => (
                400,
                "hekma_bad_target",
                "bad_target",
                "This request target cannot be forwarded: send an origin-form path to the \
                 metering proxy.",
                Some(false),
            ),
            WireError::BadHost => (
                403,
                "hekma_bad_host",
                "bad_host",
                "This address is not an allowed Host for the metering proxy.",
                Some(false),
            ),
            WireError::PathNotAllowed => (
                400,
                "hekma_path_not_allowed",
                "path_not_allowed",
                "This path is outside the endpoints allowed for the held provider key. Ask the \
                 operator to widen the key's path list if this endpoint is genuinely needed.",
                Some(false),
            ),
            WireError::UnscentedCall => (
                401,
                "hekma_unscented_call",
                "unscented_call",
                "This call carries no credential Hekma issued, and its Run's Scent-Check Mode is \
                 enforce. Send the Run Key in x-hekma-run-key, or use key custody.",
                Some(false),
            ),
            WireError::UnknownCredential => (
                401,
                "hekma_unknown_credential",
                "unknown_credential",
                "This credential is not one Hekma honours. Check the Run Key you were given.",
                Some(false),
            ),
            WireError::CredentialRevoked => (
                401,
                "hekma_credential_revoked",
                "credential_revoked",
                "This Run Key was revoked. Stop using it; ask for a new one.",
                Some(false),
            ),
            WireError::RunOutOfScope => (
                403,
                "hekma_run_out_of_scope",
                "run_out_of_scope",
                "This Run Key's Run is not this route's Run or beneath it.",
                Some(false),
            ),
            WireError::NoRoute => (
                404,
                "hekma_no_route",
                "no_route",
                "No binding matches this path. Create one with hekma binding create.",
                Some(false),
            ),
            WireError::BodyTooLarge => (
                413,
                "hekma_body_too_large",
                "body_too_large",
                "The request body is larger than the metering proxy accepts.",
                Some(false),
            ),
            WireError::ProxyBusy => (
                503,
                "hekma_proxy_busy",
                "proxy_busy",
                "The metering proxy is at its concurrency limit. Retry shortly.",
                Some(true),
            ),
            WireError::LedgerUnavailable => (
                503,
                "hekma_ledger_unavailable",
                "ledger_unavailable",
                "Hekma could not record this call, so it was not sent. Retry shortly.",
                Some(true),
            ),
            WireError::RunUnavailable => (
                503,
                "hekma_run_unavailable",
                "run_unavailable",
                "Hekma could not create the Run for this call in time, so it was not sent. \
                 Retry shortly.",
                Some(true),
            ),
            WireError::UpstreamError => (
                502,
                "hekma_upstream_error",
                "upstream_error",
                "The upstream request failed.",
                None,
            ),
            WireError::RunReject => (
                402,
                "hekma_run_reject",
                "run_reject",
                "This Run's model calls are rejected by its operator. Do not retry.",
                Some(false),
            ),
            WireError::HoldFull => (
                503,
                "hekma_hold_full",
                "hold_full",
                "This Run's calls are held and its hold queue is full. Retry shortly.",
                Some(true),
            ),
            WireError::HoldInterrupted => (
                503,
                "hekma_hold_interrupted",
                "hold_interrupted",
                "This call was held when the metering proxy stopped. Retry it; it will be held \
                 again.",
                Some(true),
            ),
        }
    }

    /// The canonical body bytes.
    pub(crate) fn body(self) -> String {
        let (_, error_type, code, message, _) = self.row();
        format!(
            "{{\"type\":\"error\",\"error\":{{\"type\":\"{error_type}\",\"code\":\"{code}\",\
             \"message\":\"{message}\",\"param\":null,\"proxy_wire\":\"{PROXY_WIRE_VERSION}\"}}}}"
        )
    }

    /// The full response.
    pub(crate) fn response(self) -> Response<Full<Bytes>> {
        let (status, _, _, _, retry) = self.row();
        let mut response = Response::new(Full::new(Bytes::from(self.body())));
        *response.status_mut() =
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let headers = response.headers_mut();
        headers.insert(
            hyper::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if let Some(retry) = retry {
            headers.insert(
                HeaderName::from_static("x-should-retry"),
                HeaderValue::from_static(if retry { "true" } else { "false" }),
            );
            if retry {
                headers.insert(hyper::header::RETRY_AFTER, HeaderValue::from_static("1"));
            }
        }
        response
    }
}

/// The fixed, content-free `error.message` for a `block` refusal (19-2,
/// unchanged).
fn refusal_message(refusal: &FenceRefusal) -> String {
    let scope = match refusal.scope {
        BreachScope::PerRun => "per-run",
        BreachScope::Cumulative => "cumulative",
    };
    let dimension = match refusal.dimension {
        BreachDimension::Tokens => "token",
        BreachDimension::Dollars => "dollar",
    };
    format!(
        "Hekma budget fence: this agent's {scope} {dimension} ceiling is reached, so further \
         model calls are refused. This is not retryable; the operator raises the ceiling or \
         starts a new Run."
    )
}

/// The 19-2 `block` refusal body bytes, unchanged (compact JSON, fixed
/// field order).
pub(crate) fn block_body(refusal: &FenceRefusal) -> String {
    format!(
        "{{\"type\":\"error\",\"error\":{{\"type\":\"{error_type}\",\"code\":\"{code}\",\
         \"message\":\"{message}\",\"param\":null,\"dimension\":\"{dimension}\",\
         \"scope\":\"{scope}\",\"proxy_wire\":\"{wire}\"}}}}",
        error_type = refusal.error_type,
        code = refusal.code,
        message = refusal_message(refusal),
        dimension = refusal.dimension.as_str(),
        scope = refusal.scope.as_str(),
        wire = PROXY_WIRE_VERSION,
    )
}

/// The 19-2 `block` refusal response (402, `x-should-retry: false`, no
/// `retry-after`).
pub(crate) fn block_response(refusal: &FenceRefusal) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(block_body(refusal))));
    *response.status_mut() =
        StatusCode::from_u16(refusal.status).unwrap_or(StatusCode::PAYMENT_REQUIRED);
    let headers = response.headers_mut();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        HeaderName::from_static("x-should-retry"),
        HeaderValue::from_static("false"),
    );
    response
}

/// Whether a request method is exempt from `block`: the safe methods carry
/// no billable completion.
pub(crate) fn is_block_exempt(method: &Method) -> bool {
    *method == Method::GET || *method == Method::HEAD || *method == Method::OPTIONS
}

/// The `x-hekma-breach` value for latched reasons: their codes in the
/// canonical order, comma-separated.
pub(crate) fn breach_header_value(reasons: &[LatchKey]) -> Option<HeaderValue> {
    if reasons.is_empty() {
        return None;
    }
    let codes: Vec<&str> = reasons
        .iter()
        .map(|reason| crate::domain::fence_code(*reason))
        .collect();
    HeaderValue::from_str(&codes.join(", ")).ok()
}

/// Remove every `x-hekma-*` request header (the strip rule).
pub(crate) fn strip_hekma_headers(headers: &mut HeaderMap) {
    let names: Vec<HeaderName> = headers
        .keys()
        .filter(|name| name.as_str().starts_with(STRIP_PREFIX))
        .cloned()
        .collect();
    for name in names {
        headers.remove(&name);
    }
}

/// Percent-encode one label key or value for `x-hekma-labels` (unreserved
/// characters kept).
fn encode_label_part(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/' | b':') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The outbound `x-hekma-labels` value: the Run's labels as sorted,
/// percent-encoded `k=v` items, comma-separated (`None` with no label).
pub(crate) fn labels_header_value(labels: &[(String, String)]) -> Option<HeaderValue> {
    if labels.is_empty() {
        return None;
    }
    let mut items: Vec<String> = labels
        .iter()
        .map(|(key, value)| format!("{}={}", encode_label_part(key), encode_label_part(value)))
        .collect();
    items.sort();
    HeaderValue::from_str(&items.join(",")).ok()
}

/// Set a first announce's credentials on the call's response (story
/// 23-2): the Run Key and refresh credential as SENSITIVE headers (no
/// `Debug` path prints them), and `cache-control: no-store` — a one-time
/// credential must not sit in a cache. Their only cleartext crossing on
/// this channel.
pub(crate) fn set_announce_headers(
    response: &mut hyper::Response<http_body_util::Full<bytes::Bytes>>,
    issued: &crate::domain::credential::IssuedCredentials,
) {
    use hyper::header::{HeaderValue, CACHE_CONTROL};
    let headers = response.headers_mut();
    for (name, value) in [
        (RUN_KEY_RESPONSE_HEADER, issued.run_key()),
        (REFRESH_RESPONSE_HEADER, issued.refresh_credential()),
    ] {
        if let Ok(mut value) = HeaderValue::from_str(value.expose_secret()) {
            value.set_sensitive(true);
            headers.insert(name, value);
        }
    }
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_refusal_row_is_well_formed_and_unique() {
        let mut codes = std::collections::HashSet::new();
        for error in WireError::ALL {
            let (status, error_type, code, message, retry) = error.row();
            assert!(codes.insert(code), "{code} appears twice");
            assert_eq!(error_type, format!("hekma_{code}"), "{error:?}");
            assert!((400..600).contains(&status), "{error:?}");
            // Only a 503 asks for a retry; the upstream's 502 leaves the
            // SDK's own default.
            assert_eq!(retry == Some(true), status == 503, "{error:?}");
            assert_eq!(retry.is_none(), error == WireError::UpstreamError);
            assert!(
                !message.contains('"') && !message.contains('\\'),
                "{error:?}"
            );

            let body: serde_json::Value = serde_json::from_str(&error.body()).unwrap();
            assert_eq!(body["type"], "error");
            assert_eq!(body["error"]["type"], error_type);
            assert_eq!(body["error"]["code"], code);
            assert_eq!(body["error"]["message"], message);
            assert!(body["error"]["param"].is_null());
            assert_eq!(body["error"]["proxy_wire"], PROXY_WIRE_VERSION);

            let response = error.response();
            assert_eq!(response.status().as_u16(), status);
            let headers = response.headers();
            assert_eq!(headers[hyper::header::CONTENT_TYPE], "application/json");
            let should_retry = headers.get("x-should-retry").map(|v| v.to_str().unwrap());
            assert_eq!(
                should_retry,
                retry.map(|r| if r { "true" } else { "false" })
            );
            assert_eq!(
                headers.contains_key(hyper::header::RETRY_AFTER),
                retry == Some(true),
                "{error:?}"
            );
        }
        assert_eq!(codes.len(), WireError::ALL.len());
    }
}
