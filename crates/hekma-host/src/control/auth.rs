//! The Control API's bearer-auth layer (story 17-3): ONE tower-style
//! middleware over every route, comparing the presented `Authorization:
//! Bearer` token against the secret the host resolved at STARTUP through the
//! engine's `SecretResolver` composite (env → the state-dir secrets file —
//! AD-10; the host never builds a second resolution mechanism).
//!
//! No-leak discipline (NFR-6 / the frozen acceptance criteria): the secret's
//! VALUE lives only inside a [`BearerToken`] (a [`SecretString`] wrapper
//! whose `Debug`/`Display` redact); the compare is CONSTANT-TIME over the
//! token bytes (never an early content return); a 401 never echoes the
//! presented value (the body is static — see [`super::error`]); no response,
//! log line, or SSE frame ever carries the secret.

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hekma_engine::domain::SecretString;

use super::error::ApiError;
use super::ApiState;

/// The startup-resolved bearer secret. `SecretString` is deliberately not
/// `Clone`, so the token wraps it in an `Arc` — one allocation, shared by
/// every request task. Construction is the ONLY way a value enters; the
/// single greppable exposure site is inside [`BearerToken::matches`] (its
/// `expose_secret()` call feeding the constant-time compare) — no other read
/// exists.
#[derive(Clone)]
pub(crate) struct BearerToken {
    secret: std::sync::Arc<SecretString>,
}

impl BearerToken {
    /// Wrap the startup-resolved secret.
    pub(crate) fn new(secret: SecretString) -> Self {
        BearerToken {
            secret: std::sync::Arc::new(secret),
        }
    }

    /// Whether the presented `Authorization: Bearer` value (when one was
    /// presented at all) matches the resolved secret. `None` (no bearer
    /// value extracted) never matches. This method holds the ONE
    /// `expose_secret()` read in the codebase — the grep anchor.
    pub(crate) fn matches(&self, presented: Option<&str>) -> bool {
        match presented {
            Some(value) => {
                constant_time_eq(value.as_bytes(), self.secret.expose_secret().as_bytes())
            }
            None => false,
        }
    }
}

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the value — not even a length hint (NFR-6).
        f.write_str("BearerToken(<redacted>)")
    }
}

/// Byte-equal in time independent of WHERE the first difference is (fold-XOR
/// over the full extent; the only early signal a caller could observe is
/// length, which the HTTP header framing already discloses — content never
/// influences control flow).
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let bound = a.len().max(b.len());
    for index in 0..bound {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        diff |= (left ^ right) as usize;
    }
    diff == 0
}

/// Extract the bearer value from an `Authorization: Bearer <value>` header —
/// `None` when the header is absent, non-UTF-8, not the Bearer scheme, or
/// presented AMBIGUOUSLY (more than one `Authorization` field — RFC 9110's
/// recipient rule: several credentials cannot be disambiguated, so the
/// request is refused, never first-wins).
fn presented_bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = match (values.next(), values.next()) {
        (Some(one), None) => one,
        // Zero or multiple Authorization fields: not a presentation.
        _ => return None,
    };
    let value = value.to_str().ok()?;
    // The auth-scheme token is CASE-INSENSITIVE (RFC 9110 §11): `bearer`,
    // `BEARER`, and `Bearer` are the same scheme.
    let (scheme, rest) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    // Extra SP between scheme and token is legal (1*SP); an EMPTY token is
    // not a presentation (it would also never match — skipping the compare
    // is free and explicit).
    let token = rest.trim_start_matches(' ');
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// The ONE auth gate over every route (`middleware::from_fn_with_state`):
/// a request that does not present the resolved bearer is answered 401
/// (static body + `WWW-Authenticate: Bearer`) WITHOUT reaching any handler —
/// the I/O matrix's "no handler runs".
pub(crate) async fn require_bearer(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Response {
    if !state.bearer.matches(presented_bearer(request.headers())) {
        return ApiError::unauthorized().into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_tokens_match_and_differing_tokens_do_not() {
        let token = BearerToken::new(SecretString::new("correct-horse".to_string()));
        assert!(token.matches(Some("correct-horse")));
        assert!(!token.matches(Some("wrong-horse")));
        assert!(!token.matches(Some("correct-horse ")));
        assert!(!token.matches(Some("")));
        // No presentation at all never matches.
        assert!(!token.matches(None));
    }

    #[test]
    fn length_differences_never_match_but_are_compared_in_constant_content_time() {
        // The fold covers the longer extent: a long attacker token against a
        // short secret is one full pass, not an early length bail with
        // content influence.
        assert!(!constant_time_eq(b"short", b"a-much-longer-attacker-token"));
        assert!(!constant_time_eq(b"a-much-longer-attacker-token", b"short"));
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"same!"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn the_debug_form_never_carries_the_value() {
        let token = BearerToken::new(SecretString::new("super-secret-value".to_string()));
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    #[test]
    fn the_bearer_header_parse_accepts_only_the_bearer_scheme_with_a_value() {
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(presented_bearer(&headers), None);

        headers.insert(
            AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer tok-1"),
        );
        assert_eq!(presented_bearer(&headers), Some("tok-1"));

        // A wrong scheme is not a presentation.
        headers.insert(
            AUTHORIZATION,
            axum::http::HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        assert_eq!(presented_bearer(&headers), None);

        // A bare `Bearer` with no value is not a presentation either.
        headers.insert(
            AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer "),
        );
        assert_eq!(presented_bearer(&headers), None);

        // RFC 9110 §11: the scheme is CASE-INSENSITIVE.
        headers.insert(
            AUTHORIZATION,
            axum::http::HeaderValue::from_static("bearer tok-2"),
        );
        assert_eq!(presented_bearer(&headers), Some("tok-2"));
        headers.insert(
            AUTHORIZATION,
            axum::http::HeaderValue::from_static("BEARER tok-3"),
        );
        assert_eq!(presented_bearer(&headers), Some("tok-3"));

        // MULTIPLE Authorization fields are ambiguous — refused (never
        // first-wins).
        headers.insert(
            AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer tok-4"),
        );
        headers.append(
            AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer tok-5"),
        );
        assert_eq!(presented_bearer(&headers), None);
    }
}
