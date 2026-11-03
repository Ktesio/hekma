//! The Control API's auth layer (story 17-3; story 21-4 made it AD-20's
//! credential resolver): ONE middleware over every route, resolving the
//! presented `Authorization: Bearer` credential against the host's
//! [`CredentialResolver`] and checking the resolved class against the
//! authority matrix ([`CredentialClass::allows`]).
//!
//! The resolver holds NO credential value — only the SHA-256 of each one
//! with its class (AD-22: every credential is looked up by hash). The local
//! operator token (minted per serve, written to `control.json`) and the
//! `--bearer-secret` value (resolved at startup through the engine's
//! `SecretResolver` composite, AD-10) are both operator/Host bearer
//! credentials — the only class issued today; the other AD-20 classes
//! (announce-scoped, Run Key, refresh, proxy-access, Node) land with their
//! stories (22-1, 22-3, 24-2, the Hub story).
//!
//! No-leak discipline (NFR-6): the presented value is hashed and compared in
//! CONSTANT TIME against EVERY entry (no early exit — the number of entries
//! and their order never shape the timing by content); a 401 never echoes
//! it (the body is static — see [`super::error`]); no response, log line or
//! SSE frame carries a credential.

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::error::ApiError;
use super::ApiState;

/// The credential classes of AD-20's authority matrix. Only the
/// operator/Host bearer is issued today.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialClass {
    /// The operator/Host bearer: the local operator token and the
    /// `--bearer-secret` value. Every Control API route; never accepted by
    /// the Metering Proxy and never handed to an agent.
    OperatorBearer,
}

/// The surfaces a credential may be presented on (the matrix's columns).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Surface {
    /// Every `/v0` Control API route.
    ControlApi,
}

impl CredentialClass {
    /// The authority matrix (`class × surface`). A class refused on a surface
    /// answers the typed `403 forbidden`; with the one class issued today,
    /// every presented-and-resolved credential is allowed.
    pub(crate) fn allows(self, surface: Surface) -> bool {
        match (self, surface) {
            (CredentialClass::OperatorBearer, Surface::ControlApi) => true,
        }
    }
}

/// The host's credential resolver: `(sha256(credential), class)` entries,
/// shared by every listener (the local endpoint and `--listen`).
#[derive(Clone, Default)]
pub(crate) struct CredentialResolver {
    entries: std::sync::Arc<Vec<([u8; 32], CredentialClass)>>,
}

impl CredentialResolver {
    /// A resolver over `(credential, class)` pairs — each credential is
    /// hashed here and its value is never kept.
    pub(crate) fn new<'a>(
        credentials: impl IntoIterator<Item = (&'a str, CredentialClass)>,
    ) -> Self {
        let entries = credentials
            .into_iter()
            .map(|(value, class)| (sha256(value.as_bytes()), class))
            .collect();
        CredentialResolver {
            entries: std::sync::Arc::new(entries),
        }
    }

    /// Resolve a presented credential to its class. `None` (nothing
    /// presented) never resolves. The presented value is hashed once and
    /// compared in constant time against EVERY entry — the loop never exits
    /// early on a match.
    pub(crate) fn resolve(&self, presented: Option<&str>) -> Option<CredentialClass> {
        let presented = presented?;
        let digest = sha256(presented.as_bytes());
        let mut found = None;
        for (hash, class) in self.entries.iter() {
            if constant_time_eq(&digest, hash) && found.is_none() {
                found = Some(*class);
            }
        }
        found
    }
}

impl std::fmt::Debug for CredentialResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never a hash, never a value — the count is a configuration fact.
        write!(f, "CredentialResolver({} credentials)", self.entries.len())
    }
}

/// The SHA-256 of `bytes` (the vendored ring digest — no new package).
fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
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
    // not a presentation.
    let token = rest.trim_start_matches(' ');
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// The ONE auth gate over every route (`middleware::from_fn_with_state`):
/// a request whose credential does not resolve is answered 401 (a static
/// body and `WWW-Authenticate: Bearer`) WITHOUT reaching any handler; a
/// resolved credential whose class the Control API does not allow is the
/// typed 403 (reserved — see [`ApiError::forbidden`]). An authenticated
/// request is counted as host activity (the auto-started host's idle clock,
/// story 21-4).
pub(crate) async fn require_bearer(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Response {
    match state.resolver.resolve(presented_bearer(request.headers())) {
        None => return ApiError::unauthorized().into_response(),
        Some(class) if !class.allows(Surface::ControlApi) => {
            return ApiError::forbidden().into_response();
        }
        Some(_) => {}
    }
    let _active = state.activity.enter();
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_presented_credential_resolves_to_its_class_and_nothing_else_does() {
        let resolver = CredentialResolver::new([
            ("local-token", CredentialClass::OperatorBearer),
            ("correct-horse", CredentialClass::OperatorBearer),
        ]);
        assert_eq!(
            resolver.resolve(Some("local-token")),
            Some(CredentialClass::OperatorBearer)
        );
        assert_eq!(
            resolver.resolve(Some("correct-horse")),
            Some(CredentialClass::OperatorBearer)
        );
        assert_eq!(resolver.resolve(Some("wrong-horse")), None);
        assert_eq!(resolver.resolve(Some("correct-horse ")), None);
        assert_eq!(resolver.resolve(Some("")), None);
        // No presentation at all never resolves.
        assert_eq!(resolver.resolve(None), None);
        // An empty resolver resolves nothing.
        assert_eq!(CredentialResolver::default().resolve(Some("x")), None);
    }

    #[test]
    fn the_matrix_allows_the_operator_bearer_on_the_control_api() {
        assert!(CredentialClass::OperatorBearer.allows(Surface::ControlApi));
    }

    #[test]
    fn length_differences_never_match_but_are_compared_in_constant_content_time() {
        assert!(!constant_time_eq(b"short", b"a-much-longer-attacker-token"));
        assert!(!constant_time_eq(b"a-much-longer-attacker-token", b"short"));
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"same!"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn the_debug_form_never_carries_a_value_or_a_hash() {
        let resolver =
            CredentialResolver::new([("super-secret-value", CredentialClass::OperatorBearer)]);
        let rendered = format!("{resolver:?}");
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
        assert_eq!(rendered, "CredentialResolver(1 credentials)");
    }

    #[test]
    fn the_hash_is_sha256() {
        // The FIPS 180-2 "abc" vector.
        let digest = sha256(b"abc");
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
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
