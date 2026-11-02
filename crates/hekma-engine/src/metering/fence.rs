//! The WIRE FENCE (story 19-2, spine AD-7 "`block` before the shared proxy")
//! — the per-instance, lock-free flag a `block` Breach Action raises and the
//! loopback listener's `forward()` checks on every request.
//!
//! ## Shape
//!
//! A [`WireFence`] is an `Arc<AtomicU8>` reason bitmask: one bit per
//! `(dimension, scope)` in [`FENCE_REASON_ORDER`]. The AD-7 pipeline raises a
//! bit (`fetch_or`, Release) after it RECORDS a `block` breach; `forward()`
//! loads it (Acquire) and, when any bit is set, answers with the refusal
//! [`fence_refusal`] decides — the same pure consumer the governance vectors
//! pin. No lock and no allocation on the pass path (AD-18: nothing in the
//! request path waits on the engine).
//!
//! Bits are only ever SET: a fence lives exactly as long as its listener,
//! which lives for one Run, so every reason clears with the Run (a raised
//! ceiling takes effect at the next Run; the live un-fence on a raise lands
//! with story 21.6's within-Run re-arm).
//!
//! ## The refusal — `proxy-wire v1`, first slice
//!
//! HTTP **402**, `content-type: application/json`, `x-should-retry: false`
//! (the OpenAI and Anthropic SDKs honour it ahead of the status), no
//! `retry-after`, and a compact body that parses as both an OpenAI and an
//! Anthropic error:
//!
//! ```json
//! {"type":"error","error":{"type":"hekma_budget_block","code":"tokens_per_run",
//!  "message":"…","param":null,"dimension":"tokens","scope":"per-run","proxy_wire":"v1"}}
//! ```
//!
//! The body names the reason only — never a limit, a total, an instance
//! name, or any request/response content (the no-leak discipline). Safe
//! methods (GET/HEAD/OPTIONS) carry no billable completion and pass, so a
//! fenced agent's model listing and probes keep working and it meets the
//! refusal on its next model call.
//!
//! NFR-6: the fence is cooperative — it refuses calls routed through the
//! listener; an agent that reaches the provider directly is not fenced.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Response};

use crate::domain::{
    fence_refusal, BreachDimension, BreachScope, FenceRefusal, LatchKey, FENCE_REASON_ORDER,
    PROXY_WIRE_VERSION,
};

/// The per-instance wire fence (story 19-2): a lock-free reason bitmask,
/// shared between the listener's request path and the supervisor's AD-7
/// pipeline. Cloning shares the same flag.
#[derive(Clone, Debug, Default)]
pub(crate) struct WireFence(Arc<AtomicU8>);

impl WireFence {
    /// RAISE the fence for `reason` (idempotent — a set bit stays set).
    /// Only the AD-7 pipeline calls this: `apply_breach`'s `block` arm, the
    /// Run-start evaluation, and the adoption rebuild from the breach log.
    pub(crate) fn raise(&self, reason: LatchKey) {
        self.0.fetch_or(reason_bit(reason), Ordering::Release);
    }

    /// The raised reasons, in [`FENCE_REASON_ORDER`] (the tests' view; the
    /// request path goes through [`Self::refusal`]).
    #[cfg(test)]
    pub(crate) fn reasons(&self) -> Vec<LatchKey> {
        reasons_from_bits(self.0.load(Ordering::Acquire))
    }

    /// The refusal a request gets now, or `None` when the fence is down. The
    /// down path is one atomic load.
    pub(crate) fn refusal(&self) -> Option<FenceRefusal> {
        let bits = self.0.load(Ordering::Acquire);
        if bits == 0 {
            return None;
        }
        fence_refusal(&reasons_from_bits(bits))
    }
}

/// The bit for one reason: its index in [`FENCE_REASON_ORDER`], which lists
/// every `(dimension, scope)` pair.
fn reason_bit(reason: LatchKey) -> u8 {
    let index = FENCE_REASON_ORDER
        .iter()
        .position(|candidate| *candidate == reason)
        .expect("FENCE_REASON_ORDER lists every (dimension, scope) pair");
    1 << index
}

fn reasons_from_bits(bits: u8) -> Vec<LatchKey> {
    FENCE_REASON_ORDER
        .iter()
        .enumerate()
        .filter(|(index, _)| bits & (1 << index) != 0)
        .map(|(_, reason)| *reason)
        .collect()
}

/// Whether a request method is exempt from the fence: the safe methods carry
/// no billable completion. (A response to one is still skimmed for `usage`
/// like any other, so passing it never hides spend from the ledger.)
pub(crate) fn is_fence_exempt(method: &Method) -> bool {
    *method == Method::GET || *method == Method::HEAD || *method == Method::OPTIONS
}

/// The fixed, content-free `error.message` for a refusal.
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

/// The canonical refusal body bytes (compact JSON, fixed field order). Every
/// interpolated value is a fixed engine token, so no escaping is needed and
/// no traffic can reach the body.
pub(crate) fn refusal_body(refusal: &FenceRefusal) -> String {
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

/// The full refusal response a fenced request receives.
pub(crate) fn refusal_response(refusal: &FenceRefusal) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(refusal_body(refusal))));
    *response.status_mut() = hyper::StatusCode::from_u16(refusal.status)
        .expect("the fence refusal status is a valid HTTP status");
    let headers = response.headers_mut();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        hyper::header::HeaderName::from_static("x-should-retry"),
        hyper::header::HeaderValue::from_static("false"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(dimension: BreachDimension, scope: BreachScope) -> LatchKey {
        LatchKey { dimension, scope }
    }

    #[test]
    fn a_fresh_fence_is_down() {
        let fence = WireFence::default();
        assert!(fence.refusal().is_none());
        assert!(fence.reasons().is_empty());
    }

    #[test]
    fn raising_is_shared_by_clones_and_idempotent() {
        let fence = WireFence::default();
        let held_by_listener = fence.clone();
        fence.raise(key(BreachDimension::Dollars, BreachScope::Cumulative));
        fence.raise(key(BreachDimension::Dollars, BreachScope::Cumulative));
        assert_eq!(
            held_by_listener.reasons(),
            vec![key(BreachDimension::Dollars, BreachScope::Cumulative)]
        );
        let refusal = held_by_listener.refusal().expect("raised");
        assert_eq!(refusal.code, "dollars_cumulative");
    }

    #[test]
    fn the_refusal_reports_the_first_canonical_reason_like_the_pure_consumer() {
        let fence = WireFence::default();
        fence.raise(key(BreachDimension::Dollars, BreachScope::PerRun));
        fence.raise(key(BreachDimension::Tokens, BreachScope::Cumulative));
        assert_eq!(fence.refusal(), fence_refusal(&fence.reasons()));
        assert_eq!(fence.refusal().unwrap().code, "tokens_cumulative");
    }

    #[test]
    fn only_safe_methods_are_exempt() {
        for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(is_fence_exempt(&method), "{method}");
        }
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(!is_fence_exempt(&method), "{method}");
        }
    }

    #[test]
    fn the_refusal_response_is_the_proxy_wire_v1_contract() {
        let fence = WireFence::default();
        fence.raise(key(BreachDimension::Tokens, BreachScope::PerRun));
        let response = refusal_response(&fence.refusal().unwrap());
        assert_eq!(response.status(), 402);
        assert_eq!(response.headers()["content-type"], "application/json");
        assert_eq!(response.headers()["x-should-retry"], "false");
        assert!(response.headers().get("retry-after").is_none());

        let body = refusal_body(&fence.refusal().unwrap());
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(value["type"], "error");
        let error = &value["error"];
        assert_eq!(error["type"], "hekma_budget_block");
        assert_eq!(error["code"], "tokens_per_run");
        assert!(error["param"].is_null());
        assert_eq!(error["dimension"], "tokens");
        assert_eq!(error["scope"], "per-run");
        assert_eq!(error["proxy_wire"], "v1");
        let message = error["message"].as_str().unwrap();
        assert!(message.contains("per-run token ceiling"), "{message}");
        assert!(message.contains("not retryable"), "{message}");
        // The exact key order is part of the canonical bytes.
        assert!(body.starts_with(
            "{\"type\":\"error\",\"error\":{\"type\":\"hekma_budget_block\",\"code\":\"tokens_per_run\",\"message\":"
        ));
        assert!(body.ends_with(
            "\"param\":null,\"dimension\":\"tokens\",\"scope\":\"per-run\",\"proxy_wire\":\"v1\"}}"
        ));
    }

    #[test]
    fn every_reason_renders_a_valid_body() {
        for reason in FENCE_REASON_ORDER {
            let refusal = fence_refusal(&[reason]).unwrap();
            let value: serde_json::Value =
                serde_json::from_str(&refusal_body(&refusal)).expect("valid JSON");
            assert_eq!(value["error"]["code"], refusal.code);
            assert_eq!(value["error"]["scope"], reason.scope.as_str());
            assert_eq!(value["error"]["dimension"], reason.dimension.as_str());
        }
    }
}
