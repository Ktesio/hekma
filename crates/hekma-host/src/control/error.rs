//! The Control API's error mapper (story 17-3): the engine's typed errors →
//! typed HTTP statuses, exactly the spec's table —
//! `400` invalid name/body, `401` auth (see [`super::auth`]), `404` unknown
//! instance, `409` invalid transition or cursor-past-family, `500` surfaced
//! reason. Never a panic: every handler error path flows through
//! [`ApiError`]'s `IntoResponse`, and an unmapped variant lands on the honest
//! `500` catch-all with its `Display` text — never `unwrap`, never a handler
//! panic turned into a dropped connection.
//!
//! The `500` message is the error's OWN surfaced reason (the engine's
//! diagnostics are no-leak by construction — AD-10/NFR-6 — so rendering them
//! verbatim cannot leak a secret value; `reveal_secrets` is never projected,
//! so no handler ever holds cleartext to leak).
//!
//! The cursor-past-family mapping deserves its note: the engine reports a
//! resync cursor beyond a family's committed count as
//! [`hekma_engine::EngineError::Log`] (there is no dedicated variant — the
//! engine surface was additive-minor'd at story 10-3 and 17-3 adds none), so
//! the mapper recognizes the truncation guard's STABLE detail sentence (the
//! engine builds it in exactly one place, `domain::resync::
//! reject_truncated_family`) and maps it to the spec'd `409`. The unit pin
//! below and the end-to-end conformance test (a real over-run cursor through
//! a real engine answering 409) hold this coupling together — if the engine
//! ever rewords the sentence, both go red together.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use hekma_engine::{ConfigError, EngineError, RegistryError};
use serde::Serialize;

use super::dto;

/// The stable machine kinds an [`ApiError`] body can carry. One per spec'd
/// status class (plus the two fine-grained 400/409 classes the spec names),
/// frozen strings — a Host branches on these, never on prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApiErrorKind {
    /// The request body could not be honored (bad JSON, unknown key, invalid
    /// value, exactly-one-of violated).
    InvalidBody,
    /// The instance name failed the naming rule.
    InvalidName,
    /// No such instance (or resource).
    NotFound,
    /// The request conflicts with the instance's current state — the spec's
    /// "invalid transition" class plus its state-conflict siblings
    /// (already-exists, running-requires-force, not-running, ...).
    Conflict,
    /// The resync cursor is past a family's committed count (never clamped).
    CursorInvalid,
    /// The engine refused / failed the operation for a surfaced reason.
    Internal,
    /// The path exists but the METHOD does not (the router's
    /// method-not-allowed fallback).
    MethodNotAllowed,
    /// Missing or wrong bearer token.
    Unauthorized,
}

impl ApiErrorKind {
    /// The frozen wire string for the `error.kind` field.
    pub fn as_str(self) -> &'static str {
        match self {
            ApiErrorKind::InvalidBody => "invalid_body",
            ApiErrorKind::InvalidName => "invalid_name",
            ApiErrorKind::NotFound => "not_found",
            ApiErrorKind::Conflict => "conflict",
            ApiErrorKind::CursorInvalid => "cursor_invalid",
            ApiErrorKind::Internal => "internal",
            ApiErrorKind::MethodNotAllowed => "method_not_allowed",
            ApiErrorKind::Unauthorized => "unauthorized",
        }
    }

    /// The HTTP status the kind maps to.
    pub fn status(self) -> StatusCode {
        match self {
            ApiErrorKind::InvalidBody | ApiErrorKind::InvalidName => StatusCode::BAD_REQUEST,
            ApiErrorKind::NotFound => StatusCode::NOT_FOUND,
            ApiErrorKind::Conflict | ApiErrorKind::CursorInvalid => StatusCode::CONFLICT,
            ApiErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            ApiErrorKind::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            ApiErrorKind::Unauthorized => StatusCode::UNAUTHORIZED,
        }
    }
}

/// The typed error every handler returns. Converts into the versioned JSON
/// error body (`{"schema_version":..,"error":{"kind":..,"message":..}}`) with
/// the mapped status.
#[derive(Debug)]
pub(crate) struct ApiError {
    kind: ApiErrorKind,
    message: String,
    /// A status that overrides the kind's default mapping — set ONLY where
    /// the rejection's own HTTP class is more precise than the kind's (the
    /// JSON-extractor rejections keep axum's 400/415/422 distinction; review
    /// item 9). Every other construction leaves it `None`.
    status_override: Option<StatusCode>,
}

impl ApiError {
    /// An error with an explicit kind (the mapper's internal constructor).
    pub(crate) fn new(kind: ApiErrorKind, message: impl Into<String>) -> Self {
        ApiError {
            kind,
            message: message.into(),
            status_override: None,
        }
    }

    /// A request-body rejection (bad JSON / violated exactly-one-of / an
    /// out-of-bounds `window_secs`).
    pub(crate) fn invalid_body(message: impl Into<String>) -> Self {
        Self::new(ApiErrorKind::InvalidBody, message)
    }

    /// The auth layer's 401 (static message — it must never echo the
    /// presented token).
    pub(crate) fn unauthorized() -> Self {
        Self::new(
            ApiErrorKind::Unauthorized,
            "missing or invalid bearer token",
        )
    }

    /// The stable truncation-guard sentence the engine's resync reader builds
    /// (one construction site: `domain::resync::reject_truncated_family`).
    /// Matched as a SUBSTRING (the family word and counts are interpolated
    /// around it), never as the whole message.
    const RESYNC_TRUNCATION_SENTENCE: &'static str = "is past this family's committed count";

    /// The stable export-cursor refusal sentences the engine's outbox reader
    /// builds (story 17-4, one construction site each: `domain::outbox`,
    /// re-exported crate-root so the host MATCHES — never re-hardcodes — the
    /// wording). Matched as SUBSTRINGs (the counts are interpolated around
    /// them), never as whole messages.
    const OUTBOX_BELOW_FLOOR_SENTENCE: &'static str = hekma_engine::BELOW_FLOOR_SENTENCE;
    const OUTBOX_PAST_TAIL_SENTENCE: &'static str = hekma_engine::PAST_TAIL_SENTENCE;

    /// The spec's cursor rule: a cursor past a family's committed count is a
    /// typed `409`, never a clamp.
    fn is_cursor_past_family(detail: &str) -> bool {
        detail.contains(Self::RESYNC_TRUNCATION_SENTENCE)
    }

    /// The spec's export-cursor rule (story 17-4): below the retained floor
    /// (compaction dropped those entries) or past the committed tail — a
    /// typed `409 cursor_invalid`, never a clamp.
    fn is_outbox_cursor_refusal(detail: &str) -> bool {
        detail.contains(Self::OUTBOX_BELOW_FLOOR_SENTENCE)
            || detail.contains(Self::OUTBOX_PAST_TAIL_SENTENCE)
    }

    /// The versioned body this error renders — the serialization seam
    /// [`IntoResponse`] writes, unit-testable without an HTTP round-trip.
    fn body_json(&self) -> ErrorBody {
        ErrorBody {
            schema_version: dto::CONTROL_API_SCHEMA_VERSION,
            error: ErrorDetail {
                kind: self.kind.as_str(),
                message: self.message.clone(),
            },
        }
    }
}

impl From<EngineError> for ApiError {
    fn from(err: EngineError) -> Self {
        let message = err.to_string();
        match err {
            EngineError::NotFound { .. } => Self::new(ApiErrorKind::NotFound, message),
            EngineError::InvalidName { .. } => Self::new(ApiErrorKind::InvalidName, message),
            // The shared transition table's rejection — the spec's 409 class.
            EngineError::InvalidTransition(_) => Self::new(ApiErrorKind::Conflict, message),
            // State/declaration conflicts: the instance exists and the name is
            // valid, but the verb cannot apply right now. Same 409 class.
            EngineError::NotRunning { .. }
            | EngineError::CapabilityUnsupported { .. }
            | EngineError::ResumeUnsupported { .. }
            | EngineError::AcpTurnInFlight { .. }
            // Drain's own running-only gate (story 18-4): the instance
            // exists, the timing is wrong — the same refused-verb conflict
            // class, never a 500.
            | EngineError::DrainRefused { .. }
            // A lifecycle command that RAN and failed (story 18-4, review
            // round 1 A7): the instance exists, the adapter's own command
            // spoke a nonzero exit / spawn failure / overrun — the verb was
            // refused by the ADAPTER, not broken in the server. The state is
            // untouched (the rejection is clean), so this is the same
            // retryable conflict class (409), never a 500: the operator can
            // fix the adapter and re-issue.
            | EngineError::PauseCommandFailed { .. } => {
                Self::new(ApiErrorKind::Conflict, message)
            }
            // A drain bound outside 1..=600_000 is the caller's value being
            // invalid → 400. Unreachable through THIS route (the handler
            // bounds-checks the body before the engine call) and through the
            // CLI (clap's range) — the mapping keeps the class honest for any
            // future programmatic surface.
            EngineError::DrainBoundInvalid { .. } => Self::new(ApiErrorKind::InvalidBody, message),
            // The resync truncation guard rides `Log` — recognize its stable
            // sentence and lift it to the spec'd 409 (never a clamp).
            EngineError::Log { detail, .. } if Self::is_cursor_past_family(&detail) => {
                Self::new(ApiErrorKind::CursorInvalid, message)
            }
            // The outbox export-cursor refusals (story 17-4) ride `Log` the
            // same way — below the retained floor or past the committed tail
            // is the spec'd 409 `cursor_invalid`, never a clamp.
            EngineError::Log { detail, .. } if Self::is_outbox_cursor_refusal(&detail) => {
                Self::new(ApiErrorKind::CursorInvalid, message)
            }
            // Every other engine error is a surfaced server-side reason (a
            // launch failure, a store/log/backend error, a listener failure):
            // 500 with the engine's own no-leak diagnostic text.
            _ => Self::new(ApiErrorKind::Internal, message),
        }
    }
}

impl From<RegistryError> for ApiError {
    fn from(err: RegistryError) -> Self {
        let message = err.to_string();
        match err {
            RegistryError::NotFound { .. } => Self::new(ApiErrorKind::NotFound, message),
            RegistryError::InvalidName { .. } => Self::new(ApiErrorKind::InvalidName, message),
            // Registration/state conflicts — the 409 class.
            RegistryError::DuplicateName { .. }
            | RegistryError::RunningRequiresForce { .. }
            | RegistryError::MemoryBackingHotSwap { .. }
            | RegistryError::MemoryBackingKindConflict { .. } => {
                Self::new(ApiErrorKind::Conflict, message)
            }
            // The caller's registration input named a bad adapter (unknown
            // native kind, missing/unreadable/invalid manifest, incompatible
            // contract, no metering source / capabilities): the client's
            // request cannot be honored → 400 with the surfaced reason.
            RegistryError::UnknownAdapterKind { .. }
            | RegistryError::ManifestNotFound { .. }
            | RegistryError::ManifestUnreadable { .. }
            | RegistryError::ManifestInvalid { .. }
            | RegistryError::ContractIncompatible { .. }
            | RegistryError::NoMeteringSource { .. }
            | RegistryError::NoCapabilities { .. } => Self::new(ApiErrorKind::InvalidBody, message),
            // Store/IO/snapshot/internal-state errors: surfaced 500s.
            _ => Self::new(ApiErrorKind::Internal, message),
        }
    }
}

impl From<ConfigError> for ApiError {
    fn from(err: ConfigError) -> Self {
        let message = err.to_string();
        match err {
            // The write-side rejections: unknown key / invalid value / shape
            // conflict are the caller's body being invalid → 400.
            ConfigError::UnknownKey { .. }
            | ConfigError::InvalidValue { .. }
            | ConfigError::WriteShapeConflict { .. } => {
                Self::new(ApiErrorKind::InvalidBody, message)
            }
            ConfigError::InvalidName { .. } => Self::new(ApiErrorKind::InvalidName, message),
            ConfigError::NotFound { .. } => Self::new(ApiErrorKind::NotFound, message),
            ConfigError::MalformedLayer { .. }
            | ConfigError::Store { .. }
            | ConfigError::SecretReveal { .. } => Self::new(ApiErrorKind::Internal, message),
        }
    }
}

/// The versioned JSON error body. `kind` is the frozen machine string a Host
/// branches on; `message` is the surfaced human reason (no secret material —
/// the engine's diagnostics are no-leak by construction).
#[derive(Serialize)]
struct ErrorBody {
    schema_version: u32,
    error: ErrorDetail,
}

/// The `error` object of [`ErrorBody`].
#[derive(Serialize)]
struct ErrorDetail {
    kind: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // The extractor rejections carry their OWN status (axum's 400/415/422
        // distinction is the client-useful part); every other path uses the
        // kind's mapping.
        let status = self.status_override.unwrap_or_else(|| self.kind.status());
        let unauthorized = self.kind == ApiErrorKind::Unauthorized;
        let body = self.body_json();
        let mut response = (status, Json(body)).into_response();
        // The auth challenge: a 401 always carries the scheme (RFC 9110
        // §11.6.1) so a Host knows HOW to authenticate — without ever
        // hinting at what was wrong with the presented token (the message is
        // static; the presented value is never echoed).
        if unauthorized {
            response.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static("Bearer"),
            );
        }
        response
    }
}

/// A handler RESULT alias — every fallible handler returns this.
pub(crate) type ApiResult<T> = Result<T, ApiError>;

/// The envelope-preserving JSON extractor. A bare `Json<T>` extractor's
/// REJECTION (malformed JSON, a missing/wrong `content-type`, a body that
/// will not deserialize into the target type) would leave axum as a bare
/// `400`/`415`/`422` with no versioned body — the one crack in "every error
/// is a versioned JSON envelope" (review item 9). This wrapper runs the same
/// extraction and renders EVERY rejection through the envelope: the
/// rejection's own status is kept (it is the client-useful distinction —
/// `400` unparseable JSON vs `415` missing content-type vs `422` typed
/// mismatch), the kind is the frozen `invalid_body`, and the message is the
/// rejection's own surfaced reason (axum builds these texts for direct
/// return to clients; they carry no host secret).
pub(crate) struct ApiJson<T>(pub T);

impl<S, T> axum::extract::FromRequest<S> for ApiJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(ApiError {
                kind: ApiErrorKind::InvalidBody,
                message: rejection.body_text(),
                // The rejection's status IS the more precise class.
                status_override: Some(rejection.status()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hekma_engine::{LifecycleCommand, LifecycleError, LifecycleState, NameError};

    fn map_engine(err: EngineError) -> (StatusCode, &'static str, String) {
        let api: ApiError = err.into();
        let kind = api.kind;
        (kind.status(), kind.as_str(), api.message)
    }

    fn map_registry(err: RegistryError) -> (StatusCode, &'static str, String) {
        let api: ApiError = err.into();
        let kind = api.kind;
        (kind.status(), kind.as_str(), api.message)
    }

    fn map_config(err: ConfigError) -> (StatusCode, &'static str, String) {
        let api: ApiError = err.into();
        let kind = api.kind;
        (kind.status(), kind.as_str(), api.message)
    }

    #[test]
    fn unknown_instance_maps_404_not_found() {
        let (status, kind, _) = map_engine(EngineError::NotFound {
            name: "ghost".into(),
        });
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(kind, "not_found");
    }

    #[test]
    fn invalid_name_maps_400_invalid_name() {
        let (status, kind, message) = map_engine(EngineError::InvalidName {
            name: "Bad Name".into(),
            reason: NameError::BadFirstChar,
        });
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(kind, "invalid_name");
        assert!(message.contains("Bad Name"), "the reason is surfaced");
    }

    #[test]
    fn invalid_transition_maps_409_conflict() {
        let (status, kind, _) = map_engine(EngineError::InvalidTransition(
            LifecycleError::InvalidTransition {
                from: LifecycleState::Stopped,
                command: LifecycleCommand::Stop,
            },
        ));
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(kind, "conflict");
    }

    #[test]
    fn state_conflicts_map_409_conflict() {
        for err in [
            EngineError::NotRunning {
                name: "probe".into(),
                state: "paused".into(),
            },
            EngineError::CapabilityUnsupported {
                name: "probe".into(),
                capability: "pause".into(),
                os: "linux".into(),
                level: "unsupported".into(),
            },
            EngineError::ResumeUnsupported {
                name: "probe".into(),
                os: "linux".into(),
                level: "unsupported".into(),
            },
            EngineError::AcpTurnInFlight {
                name: "probe".into(),
            },
            // Story 18-4: drain's own running-only gate joins the class.
            EngineError::DrainRefused {
                name: "probe".into(),
                state: "stopped".into(),
            },
            // Story 18-4 review round 1 (A7): a lifecycle command that ran
            // and failed (nonzero exit / spawn failure / overrun) joins the
            // class — the ADAPTER refused the verb, the state is untouched,
            // so it is a retryable conflict, never a 500.
            EngineError::PauseCommandFailed {
                name: "probe".into(),
                direction: "pause".into(),
                command: "the-pause --graceful".into(),
                detail: "exited with code 3".into(),
            },
        ] {
            let (status, kind, _) = map_engine(err);
            assert_eq!(status, StatusCode::CONFLICT, "state conflicts are 409");
            assert_eq!(kind, "conflict");
        }
    }

    #[test]
    fn drain_bound_invalid_maps_400_invalid_body() {
        // Story 18-4: an out-of-range drain bound is the caller's VALUE being
        // wrong — the 400 class (unreachable through the HTTP route, which
        // pre-validates, and the CLI's clap range; pinned for any future
        // programmatic surface).
        let (status, kind, _) = map_engine(EngineError::DrainBoundInvalid { bound_ms: 0 });
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(kind, "invalid_body");
    }

    #[test]
    fn a_resync_cursor_past_the_family_count_maps_409_cursor_invalid() {
        // The engine's EXACT truncation-guard sentence shape (from
        // domain::resync::reject_truncated_family), with real family + counts
        // interpolated — the mapper must lift THIS Log variant to 409.
        let detail = "the resync cursor's transition position (5) is past this family's \
                      committed count (1) — the record was truncated or rotated away; \
                      resyncing would re-deliver or silently skip records, so this fails \
                      instead. Reset the cursor to ResyncCursor::START deliberately if a \
                      from-zero backfill is intended";
        let (status, kind, _) = map_engine(EngineError::Log {
            name: "probe".into(),
            path: "/state/agents/probe/instance.log".into(),
            detail: detail.into(),
        });
        assert_eq!(status, StatusCode::CONFLICT, "cursor-past-family is 409");
        assert_eq!(kind, "cursor_invalid");
    }

    #[test]
    fn an_export_cursor_below_the_retained_floor_maps_409_cursor_invalid() {
        // Story 17-4: the below-floor refusal, with the engine's own stable
        // sentence interpolated into the real message shape the outbox
        // reader builds — the mapper must lift THIS Log variant to 409 too.
        let detail = format!(
            "the export cursor (3) is {below} (7) — the journal's compaction has already \
             dropped entries at or below your cursor; exporting from it would SILENTLY \
             skip the dropped records, so this fails instead. Re-ack from the batch's \
             returned cursor, or start a fresh consumer at OutboxCursor::START \
             deliberately",
            below = hekma_engine::BELOW_FLOOR_SENTENCE,
        );
        let (status, kind, _) = map_engine(EngineError::Log {
            name: "<outbox>".into(),
            path: "/state/outbox.jsonl".into(),
            detail,
        });
        assert_eq!(status, StatusCode::CONFLICT, "below-floor is 409");
        assert_eq!(kind, "cursor_invalid");
    }

    #[test]
    fn an_export_cursor_past_the_committed_tail_maps_409_cursor_invalid() {
        // Story 17-4: the past-tail refusal (shared by the export pull and
        // the ack advance) maps to the same 409.
        let detail = format!(
            "the export cursor (99) is {past} (5) — an ack or a pull past the tail would \
             silently skip entries that have not been written yet, so this fails instead",
            past = hekma_engine::PAST_TAIL_SENTENCE,
        );
        let (status, kind, _) = map_engine(EngineError::Log {
            name: "<outbox>".into(),
            path: "/state/outbox-ack.json".into(),
            detail,
        });
        assert_eq!(status, StatusCode::CONFLICT, "past-tail is 409");
        assert_eq!(kind, "cursor_invalid");
    }

    #[test]
    fn an_ordinary_log_error_stays_500_internal() {
        let (status, kind, _) = map_engine(EngineError::Log {
            name: "probe".into(),
            path: "/state/agents/probe/instance.log".into(),
            detail: "disk full".into(),
        });
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(kind, "internal");
    }

    #[test]
    fn operational_failures_map_500_internal_with_the_surfaced_reason() {
        let (status, kind, message) = map_engine(EngineError::LaunchFailed {
            name: "probe".into(),
            detail: "no such file: missing-bin".into(),
        });
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(kind, "internal");
        assert!(
            message.contains("missing-bin"),
            "the surfaced reason rides the body"
        );
    }

    #[test]
    fn registry_errors_map_to_their_classes() {
        // Duplicate registration is a conflict.
        let (status, kind, _) = map_registry(RegistryError::DuplicateName {
            name: "probe".into(),
        });
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(kind, "conflict");

        // A running instance blocks the plain DELETE.
        let (status, kind, _) = map_registry(RegistryError::RunningRequiresForce {
            name: "probe".into(),
        });
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(kind, "conflict");

        // A bad registration body (unknown native kind) is the caller's 400.
        let (status, kind, _) = map_registry(RegistryError::UnknownAdapterKind {
            kind: "nope".into(),
        });
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(kind, "invalid_body");

        // Store failures surface as 500.
        let (status, kind, _) = map_registry(RegistryError::Io {
            name: "probe".into(),
            path: "/state".into(),
            source: std::io::Error::other("denied"),
        });
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(kind, "internal");
    }

    #[test]
    fn config_errors_map_to_their_classes() {
        let (status, kind, _) = map_config(ConfigError::UnknownKey {
            key: "modle".into(),
            suggestion: Some("model".into()),
        });
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(kind, "invalid_body");

        let (status, kind, _) = map_config(ConfigError::InvalidValue {
            key: "budget.tokens.per_run".into(),
            value: "lots".into(),
            reason: "expected a non-negative integer".into(),
        });
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(kind, "invalid_body");

        let (status, kind, _) = map_config(ConfigError::NotFound {
            name: "ghost".into(),
        });
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(kind, "not_found");

        let (status, kind, _) = map_config(ConfigError::Store {
            name: "probe".into(),
            detail: "locked".into(),
        });
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(kind, "internal");
    }

    #[test]
    fn the_error_body_is_the_versioned_json_envelope_and_never_panics() {
        // The serialization seam: status class + the versioned body shape a
        // Host branches on — kind is the frozen machine string, message the
        // surfaced reason (a text-heavy 500 serializes; never a panic path).
        let api = ApiError::new(ApiErrorKind::Internal, "boom");
        assert_eq!(api.kind.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let json = serde_json::to_value(api.body_json()).unwrap();
        assert_eq!(json["schema_version"], dto::CONTROL_API_SCHEMA_VERSION);
        assert_eq!(json["error"]["kind"], "internal");
        assert_eq!(json["error"]["message"], "boom");
    }

    #[test]
    fn the_unauthorized_body_is_static_and_never_echoes_input() {
        let api = ApiError::unauthorized();
        assert_eq!(api.kind.status(), StatusCode::UNAUTHORIZED);
        let body = serde_json::to_string(&api.body_json()).unwrap();
        // The message is the static refusal; a presented token value would
        // never enter it (the constructor takes no input to echo).
        assert!(!body.contains("sk-attacker-token"), "no echo: {body}");
        assert!(body.contains("unauthorized"), "{body}");
        assert!(body.contains("missing or invalid bearer token"), "{body}");
    }
}
