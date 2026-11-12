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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::Method;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hekma_engine::{CredentialMatch, PresentedState, SecretString, TokenClass};

use super::error::ApiError;
use super::ApiState;

/// The credential classes of AD-20's authority matrix (story 22-1 adds the
/// Run Key, refresh and announce classes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialClass {
    /// The operator/Host bearer: the local operator token and the
    /// `--bearer-secret` value. Never accepted by the Metering Proxy and
    /// never handed to an agent.
    OperatorBearer,
    /// A Run's own credential (`hk_run_`): a Caller on its Run's subtree.
    RunKey,
    /// A single-use refresh credential (`hk_ref_`).
    Refresh,
    /// An announce credential (`hk_ann_`): no route yet takes one.
    Announce,
}

impl CredentialClass {
    /// Every class (the matrix's rows).
    #[cfg(test)]
    pub(crate) const ALL: [CredentialClass; 4] = [
        CredentialClass::OperatorBearer,
        CredentialClass::RunKey,
        CredentialClass::Refresh,
        CredentialClass::Announce,
    ];

    /// The class of an engine-issued token.
    pub(crate) fn of_token(class: TokenClass) -> Self {
        match class {
            TokenClass::RunKey => CredentialClass::RunKey,
            TokenClass::Refresh => CredentialClass::Refresh,
            TokenClass::Announce => CredentialClass::Announce,
        }
    }
}

/// The route families a credential may be presented on (the matrix's
/// columns, story 22-1). `Proxy`, `Heartbeat` and `OtlpIntake` have no
/// Control API route yet; their cells are pinned for the stories that add
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // `Proxy`, `Heartbeat`, `OtlpIntake`: pinned, no route yet.
pub(crate) enum RouteFamily {
    /// Every route that is not one of the Caller families: the fleet,
    /// instances, config, labels, Policies, the listing, a Run's read,
    /// events, export, the set verbs, revoke-keys and keys.
    Operator,
    /// `POST /v0/runs` — open a child Run.
    RunOpen,
    /// `POST /v0/runs/{id}/close` — close a child Run.
    RunClose,
    /// `PATCH /v0/runs/{id}/limits` — set Run-level limits.
    RunLimits,
    /// `POST /v0/runs/refresh` — trade a refresh credential.
    RunRefresh,
    /// The shared Metering Proxy (no consumer until it exists).
    Proxy,
    /// A Run's heartbeat (with Leases).
    Heartbeat,
    /// OTLP intake.
    OtlpIntake,
}

impl RouteFamily {
    /// Every family (the matrix's columns).
    #[cfg(test)]
    pub(crate) const ALL: [RouteFamily; 8] = [
        RouteFamily::Operator,
        RouteFamily::RunOpen,
        RouteFamily::RunClose,
        RouteFamily::RunLimits,
        RouteFamily::RunRefresh,
        RouteFamily::Proxy,
        RouteFamily::Heartbeat,
        RouteFamily::OtlpIntake,
    ];

    /// The family a Control API request belongs to, by method and path.
    pub(crate) fn of(method: &Method, path: &str) -> Self {
        let segments: Vec<&str> = path.trim_end_matches('/').split('/').collect();
        match (method, segments.as_slice()) {
            (&Method::POST, ["", "v0", "runs"]) => RouteFamily::RunOpen,
            (&Method::POST, ["", "v0", "runs", "refresh"]) => RouteFamily::RunRefresh,
            (&Method::POST, ["", "v0", "runs", _, "close"]) => RouteFamily::RunClose,
            (&Method::PATCH, ["", "v0", "runs", _, "limits"]) => RouteFamily::RunLimits,
            _ => RouteFamily::Operator,
        }
    }
}

impl CredentialClass {
    /// The authority matrix (`class × route family`, R1.11) — ONE table. A
    /// class refused on a family answers `403 forbidden`; a Run Key acting
    /// outside its own subtree is the job's `403 run-out-of-scope`.
    pub(crate) fn allows(self, family: RouteFamily) -> bool {
        use CredentialClass as C;
        use RouteFamily as F;
        match (self, family) {
            (C::OperatorBearer, F::Operator | F::RunOpen | F::RunClose | F::RunLimits) => true,
            (C::OperatorBearer, F::RunRefresh | F::Proxy | F::Heartbeat | F::OtlpIntake) => false,
            (C::RunKey, F::RunOpen | F::RunClose | F::RunLimits | F::Proxy) => true,
            (C::RunKey, F::Operator | F::RunRefresh | F::Heartbeat | F::OtlpIntake) => false,
            (C::Refresh, F::RunRefresh) => true,
            (C::Refresh, _) => false,
            (C::Announce, F::Heartbeat | F::OtlpIntake) => true,
            (C::Announce, _) => false,
        }
    }
}

/// Who a request acts as (inserted by [`require_bearer`] on every request
/// it lets through).
#[derive(Clone, Debug)]
pub(crate) enum Authority {
    /// The operator/Host bearer.
    Operator,
    /// A Run Key: its Run's Caller.
    Caller(CredentialMatch),
    /// A refresh credential, passed to the refresh job (which reads the
    /// store: refresh credentials are never in the snapshot).
    Refresh(Arc<SecretString>),
}

impl Authority {
    /// The engine actor this authority acts as.
    pub(crate) fn actor(&self) -> hekma_engine::RunActor {
        match self {
            Authority::Operator | Authority::Refresh(_) => hekma_engine::RunActor::Operator,
            Authority::Caller(matched) => matched.actor(),
        }
    }
}

/// The rate-limited count of unresolved presentations (story 22-1, T5.6):
/// never a value, never an event — counts by class prefix, at most one
/// diagnostic line per interval.
#[derive(Clone)]
pub(crate) struct AuthDiagnostics {
    inner: Arc<Mutex<DiagnosticsInner>>,
    notifier: Option<Arc<crate::Notifier>>,
    interval: Duration,
}

struct DiagnosticsInner {
    counts: BTreeMap<&'static str, u64>,
    last: Option<Instant>,
}

impl AuthDiagnostics {
    /// One line per minute at most.
    pub(crate) const INTERVAL: Duration = Duration::from_secs(60);

    /// Counting into `notifier` (none: counted, never written).
    pub(crate) fn new(notifier: Option<crate::Notifier>, interval: Duration) -> Self {
        AuthDiagnostics {
            inner: Arc::new(Mutex::new(DiagnosticsInner {
                counts: BTreeMap::new(),
                last: None,
            })),
            notifier: notifier.map(Arc::new),
            interval,
        }
    }

    /// The prefix bucket of a presentation (public structure only).
    fn bucket(presented: Option<&str>) -> &'static str {
        match presented {
            None => "none",
            Some(value) => match TokenClass::of_prefix(value) {
                Some(class) => class.prefix(),
                None => "other",
            },
        }
    }

    /// Count one unresolved presentation; emit the line when the interval
    /// has passed. Returns the line emitted, if any (for tests).
    pub(crate) fn unresolved(&self, presented: Option<&str>) -> Option<String> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        *inner.counts.entry(Self::bucket(presented)).or_default() += 1;
        let due = inner
            .last
            .is_none_or(|last| last.elapsed() >= self.interval);
        if !due {
            return None;
        }
        let total: u64 = inner.counts.values().sum();
        let by_prefix = inner
            .counts
            .iter()
            .map(|(bucket, count)| format!("{bucket} {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        let line = format!(
            "the Control API refused {total} unrecognised credential presentation(s) since the \
             last report ({by_prefix}); no value is recorded"
        );
        inner.counts.clear();
        inner.last = Some(Instant::now());
        drop(inner);
        if let Some(notifier) = &self.notifier {
            notifier.note(&line);
        }
        Some(line)
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
/// resolved credential whose class the route family does not allow is the
/// typed 403. Story 22-1: an `hk_`-shaped bearer resolves through the
/// engine's lock-free snapshot ([`hekma_engine::Engine::authenticate`]),
/// with one store read on a miss to tell a revoked credential (`401
/// credential-revoked`) from an unknown one (`401 unauthorized`); a
/// supervised Run's key is refused beyond loopback; a refresh credential
/// passes to the refresh route's job. The resolved [`Authority`] rides the
/// request. An authenticated request is counted as host activity.
pub(crate) async fn require_bearer(
    State(state): State<ApiState>,
    mut request: Request,
    next: Next,
) -> Response {
    // A presented credential is a sensitive header value from here on.
    if let Some(value) = request.headers_mut().get_mut(AUTHORIZATION) {
        value.set_sensitive(true);
    }
    let family = RouteFamily::of(request.method(), request.uri().path());
    let presented = presented_bearer(request.headers()).map(str::to_string);
    let authority = match authorize(&state, presented.as_deref(), family).await {
        Ok(authority) => authority,
        Err(refusal) => return refusal.into_response(),
    };
    // Story 21-6: the presented credential's HASH rides the request (never
    // the value) — a set token is bound to the credential that minted it.
    let credential = Credential(sha256(presented.unwrap_or_default().as_bytes()));
    request.extensions_mut().insert(credential);
    request.extensions_mut().insert(authority);
    let _active = state.activity.enter();
    next.run(request).await
}

/// Resolve and authorize one presentation for `family`.
async fn authorize(
    state: &ApiState,
    presented: Option<&str>,
    family: RouteFamily,
) -> Result<Authority, ApiError> {
    let Some((class, secret)) = presented.and_then(hekma_engine::parse_presented) else {
        return match state.resolver.resolve(presented) {
            Some(class) if class.allows(family) => Ok(Authority::Operator),
            Some(_) => Err(ApiError::forbidden()),
            None => {
                state.auth_diagnostics.unresolved(presented);
                Err(ApiError::unauthorized())
            }
        };
    };
    let presented = presented.unwrap_or_default();
    if class == TokenClass::Refresh && family == RouteFamily::RunRefresh {
        return Ok(Authority::Refresh(Arc::new(secret)));
    }
    let matched = match state.engine.authenticate(presented) {
        Some(matched) => matched,
        None => match state.engine.credential_state(presented).await {
            Ok(PresentedState::Active(matched)) => matched,
            Ok(PresentedState::Revoked) => return Err(ApiError::credential_revoked()),
            Ok(PresentedState::Unknown) => {
                // An operator secret that happens to have a token's shape
                // still resolves as the operator bearer.
                if let Some(class) = state.resolver.resolve(Some(presented)) {
                    return if class.allows(family) {
                        Ok(Authority::Operator)
                    } else {
                        Err(ApiError::forbidden())
                    };
                }
                state.auth_diagnostics.unresolved(Some(presented));
                return Err(ApiError::unauthorized());
            }
            Err(err) => return Err(err.into()),
        },
    };
    let gated = gate(matched, state.loopback, family);
    if gated.as_ref().is_err_and(ApiError::is_unauthorized) {
        state.auth_diagnostics.unresolved(Some(presented));
    }
    gated
}

/// The decision over a recognised engine credential (pure): P5 first — a
/// supervised Run's key has no legitimate network use, so beyond loopback
/// it is answered as unrecognised — then the matrix.
fn gate(
    matched: CredentialMatch,
    loopback: bool,
    family: RouteFamily,
) -> Result<Authority, ApiError> {
    if matched.supervised && !loopback {
        return Err(ApiError::unauthorized());
    }
    if !CredentialClass::of_token(matched.class).allows(family) {
        return Err(ApiError::forbidden());
    }
    Ok(Authority::Caller(matched))
}

/// Refuse a credential in a URL (story 22-1, R1.12): BEFORE
/// authentication, a request whose path or query (raw, or percent-decoded)
/// carries an `hk_run_`/`hk_ref_`/`hk_ann_` token is answered `400
/// credential-in-url` with a static body. A complete token so exposed has
/// reached someone's access log, so an active one is revoked
/// (`exposed-in-url`).
pub(crate) async fn refuse_credential_in_url(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Response {
    let target = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_default();
    let scan = hekma_engine::scan_url(&target);
    if !scan.found {
        return next.run(request).await;
    }
    drop(request);
    if !scan.tokens.is_empty() {
        // Best-effort: the refusal stands whatever the revocation's fate
        // (a read-only engine, a store error).
        let _ = state.engine.revoke_exposed(scan.tokens).await;
    }
    ApiError::credential_in_url().into_response()
}

/// The SHA-256 of the credential that authenticated a request (story 21-6:
/// the set-token binding). Inserted by [`require_bearer`]; never a value.
#[derive(Clone, Copy)]
pub(crate) struct Credential(pub(crate) [u8; 32]);

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
    fn the_authority_matrix_pins_every_cell() {
        use CredentialClass as C;
        use RouteFamily as F;
        // The whole table, cell by cell (R1.11): (class, family, allowed).
        let table = [
            (C::OperatorBearer, F::Operator, true),
            (C::OperatorBearer, F::RunOpen, true),
            (C::OperatorBearer, F::RunClose, true),
            (C::OperatorBearer, F::RunLimits, true),
            (C::OperatorBearer, F::RunRefresh, false),
            (C::OperatorBearer, F::Proxy, false),
            (C::OperatorBearer, F::Heartbeat, false),
            (C::OperatorBearer, F::OtlpIntake, false),
            (C::RunKey, F::Operator, false),
            (C::RunKey, F::RunOpen, true),
            (C::RunKey, F::RunClose, true),
            (C::RunKey, F::RunLimits, true),
            (C::RunKey, F::RunRefresh, false),
            (C::RunKey, F::Proxy, true),
            (C::RunKey, F::Heartbeat, false),
            (C::RunKey, F::OtlpIntake, false),
            (C::Refresh, F::Operator, false),
            (C::Refresh, F::RunOpen, false),
            (C::Refresh, F::RunClose, false),
            (C::Refresh, F::RunLimits, false),
            (C::Refresh, F::RunRefresh, true),
            (C::Refresh, F::Proxy, false),
            (C::Refresh, F::Heartbeat, false),
            (C::Refresh, F::OtlpIntake, false),
            (C::Announce, F::Operator, false),
            (C::Announce, F::RunOpen, false),
            (C::Announce, F::RunClose, false),
            (C::Announce, F::RunLimits, false),
            (C::Announce, F::RunRefresh, false),
            (C::Announce, F::Proxy, false),
            (C::Announce, F::Heartbeat, true),
            (C::Announce, F::OtlpIntake, true),
        ];
        assert_eq!(table.len(), C::ALL.len() * F::ALL.len());
        for (class, family, allowed) in table {
            assert_eq!(class.allows(family), allowed, "{class:?} x {family:?}");
        }
    }

    #[test]
    fn a_supervised_key_is_refused_beyond_loopback_and_every_class_meets_the_matrix() {
        let matched = |class: TokenClass, supervised: bool| CredentialMatch {
            class,
            run_id: hekma_engine::RunId::from_wire("run_x"),
            token_id: "0123456789ab".to_string(),
            supervised,
            hash: hekma_engine::TokenHash::of_token("hk_run_x"),
        };
        // A supervised Run's key: fine on loopback, unrecognised beyond.
        assert!(gate(
            matched(TokenClass::RunKey, true),
            true,
            RouteFamily::RunOpen
        )
        .is_ok());
        let refused = gate(
            matched(TokenClass::RunKey, true),
            false,
            RouteFamily::RunOpen,
        )
        .unwrap_err();
        assert!(refused.is_unauthorized());
        // A Control-API-opened Run's key is accepted beyond loopback.
        assert!(gate(
            matched(TokenClass::RunKey, false),
            false,
            RouteFamily::RunOpen
        )
        .is_ok());
        // Every class × family cell through the gate agrees with the table;
        // the announce credential is refused on every Control API family.
        for class in [
            TokenClass::RunKey,
            TokenClass::Refresh,
            TokenClass::Announce,
        ] {
            for family in RouteFamily::ALL {
                let allowed = gate(matched(class, false), true, family).is_ok();
                assert_eq!(
                    allowed,
                    CredentialClass::of_token(class).allows(family),
                    "{class:?} x {family:?}"
                );
            }
        }
        for family in [
            RouteFamily::Operator,
            RouteFamily::RunOpen,
            RouteFamily::RunClose,
            RouteFamily::RunLimits,
            RouteFamily::RunRefresh,
        ] {
            assert!(gate(matched(TokenClass::Announce, false), true, family).is_err());
        }
        // The Caller authority acts as its Run's Caller.
        let authority = Authority::Caller(matched(TokenClass::RunKey, false));
        assert!(matches!(
            authority.actor(),
            hekma_engine::RunActor::Caller { .. }
        ));
        assert_eq!(
            Authority::Operator.actor(),
            hekma_engine::RunActor::Operator
        );
    }

    #[test]
    fn requests_map_to_their_route_family() {
        use RouteFamily as F;
        let cases = [
            (Method::POST, "/v0/runs", F::RunOpen),
            (Method::GET, "/v0/runs", F::Operator),
            (Method::POST, "/v0/runs/refresh", F::RunRefresh),
            (Method::POST, "/v0/runs/run_1/close", F::RunClose),
            (Method::POST, "/v0/runs/close", F::Operator),
            (Method::PATCH, "/v0/runs/run_1/limits", F::RunLimits),
            (Method::GET, "/v0/runs/run_1/limits", F::Operator),
            (Method::POST, "/v0/runs/limits", F::Operator),
            (Method::GET, "/v0/runs/run_1", F::Operator),
            (Method::GET, "/v0/runs/run_1/keys", F::Operator),
            (Method::POST, "/v0/runs/run_1/revoke-keys", F::Operator),
            (Method::POST, "/v0/runs/run_1/stop", F::Operator),
            (Method::GET, "/v0/fleet", F::Operator),
            (Method::GET, "/v0/events", F::Operator),
            (Method::POST, "/v0/runs/", F::RunOpen),
        ];
        for (method, path, family) in cases {
            assert_eq!(RouteFamily::of(&method, path), family, "{method} {path}");
        }
    }

    #[test]
    fn unresolved_presentations_are_counted_and_reported_at_most_once_per_interval() {
        let diagnostics = AuthDiagnostics::new(None, Duration::from_secs(3600));
        let first = diagnostics
            .unresolved(Some("hk_run_ABC"))
            .expect("the first reports");
        assert!(
            first.contains("refused 1") && first.contains("hk_run_ 1"),
            "{first}"
        );
        for _ in 0..1000 {
            assert!(diagnostics
                .unresolved(Some("hk_ref_SENTINELVALUE"))
                .is_none());
        }
        // A zero interval reports again, carrying the counts since.
        let quick = AuthDiagnostics::new(None, Duration::ZERO);
        let line = quick.unresolved(Some("secret-sentinel-value")).unwrap();
        assert!(
            line.contains("other 1") && !line.contains("sentinel"),
            "{line}"
        );
        let line = quick.unresolved(None).unwrap();
        assert!(line.contains("none 1"), "{line}");
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
