//! The Control API's route handlers (story 17-3): thin projections of
//! `hekma-engine`'s PUBLIC async facade — every handler calls one engine
//! method and renders the mapped response, with error conversion delegated
//! entirely to the [`ApiError`](super::error::ApiError) mapper (`?`).
//!
//! Projection discipline (AD-20 / the frozen route table): only the story's
//! named facade methods appear here. NOT projected, by design: `send_input`,
//! the restart-policy setter, the memory verbs (additive under 0.x later);
//! the log/raw-output reads (FR-41 forbids raw agent output in v0); and
//! `reveal_secrets` — NEVER, on any route, in any shape (the config read
//! renders masked values only).

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use hekma_engine::{
    AdapterRef, ConfigLayer, FleetListing, RegistryError, RemoveDisposition, ResyncCursor,
};
use serde::Deserialize;

use super::dto;
use super::error::{ApiError, ApiJson, ApiResult};
use super::ApiState;

/// `GET /v0/version` — the self-description (routes, auth, and stability
/// discovery all start here).
pub(crate) async fn version() -> Json<dto::VersionDocument> {
    Json(dto::VersionDocument {
        api: dto::API_ID,
        version: dto::API_VERSION,
        stability: dto::STABILITY,
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        host_version: env!("CARGO_PKG_VERSION"),
        engine: dto::EngineVersions {
            event: hekma_engine::EVENT_SCHEMA_VERSION,
            fleet: hekma_engine::FLEET_SCHEMA_VERSION,
            usage: hekma_engine::USAGE_SCHEMA_VERSION,
            budget: hekma_engine::BUDGET_SCHEMA_VERSION,
            log: hekma_engine::LOG_SCHEMA_VERSION,
        },
    })
}

/// `GET /v0/fleet` — the Fleet listing document, the engine's OWN versioned
/// shape (`FleetListing::new` stamps the schema version and computes the
/// fleet-wide totals from the same rows).
pub(crate) async fn fleet(State(state): State<ApiState>) -> ApiResult<Json<FleetListing>> {
    let entries = state.engine.fleet().await?;
    Ok(Json(FleetListing::new(entries)))
}

/// `GET /v0/instances/{name}` — the single-instance Fleet read (`kt agent
/// show --json`'s shape).
pub(crate) async fn show(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<dto::ShowDocument>> {
    let entry = state.engine.fleet_entry(&name).await?;
    Ok(Json(dto::ShowDocument {
        schema_version: hekma_engine::FLEET_SCHEMA_VERSION,
        instance: entry,
    }))
}

/// `GET /v0/instances/{name}/status` — the runtime status read.
pub(crate) async fn status(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<dto::StatusDocument>> {
    let status = state.engine.instance_status(&name).await?;
    Ok(Json(dto::StatusDocument {
        // ONE schema-version discipline for the whole 0.x API (review item
        // 11) — the status family carries the API-level stamp, no separate
        // counter.
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        status: status.into(),
    }))
}

/// `GET /v0/instances/{name}/capabilities` — the effective Capability
/// Declaration projected onto the current OS.
pub(crate) async fn capabilities(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<dto::CapabilitiesDocument>> {
    let capabilities = state.engine.effective_capabilities(&name).await?;
    Ok(Json(dto::CapabilitiesDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        capabilities,
    }))
}

/// `GET /v0/instances/{name}/config` — the effective config, the CLI
/// `config get --json` document (masked values; no reveal path exists here).
pub(crate) async fn config_get(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<dto::ConfigDocument>> {
    let effective = state
        .engine
        .effective_config(&name, ConfigLayer::empty())
        .await?;
    Ok(Json(dto::ConfigDocument {
        schema_version: dto::CONFIG_GET_SCHEMA_VERSION,
        entries: render_leaves(&effective),
    }))
}

/// `PUT /v0/instances/{name}/config` — one key write, acknowledged with the
/// post-write read-back PLUS the engine's zero-or-more WARN-ONLY steering
/// warnings (AI-18: they ride the body — the host's stderr is not the
/// client's). The WRITE is serialized per host through
/// [`ApiState::config_write`](super::ApiState) — the lock is held across the
/// engine write AND its read-back, so a concurrent PUT can never interleave
/// between them and every response's read-back reflects the value ITS
/// request wrote (review item 8). Reads (`GET`) stay lock-free snapshots.
pub(crate) async fn config_put(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    ApiJson(request): ApiJson<dto::ConfigPutRequest>,
) -> ApiResult<Json<dto::ConfigPutDocument>> {
    // Serialize write+read-back per host (see the doc comment).
    let _write_guard = state.config_write.lock().await;
    let warnings = state
        .engine
        .set_config(&name, &request.key, &request.value)
        .await?;
    // The read-back is the acknowledgment's payload: the config the instance
    // ACTUALLY has after the write (a put that silently returned nothing
    // would leave the client guessing what won).
    let effective = state
        .engine
        .effective_config(&name, ConfigLayer::empty())
        .await?;
    Ok(Json(dto::ConfigPutDocument {
        schema_version: dto::CONFIG_GET_SCHEMA_VERSION,
        entries: render_leaves(&effective),
        warnings,
    }))
}

/// Render the effective config into the CLI-mirroring leaf list (the ONE
/// display path — masked secrets — and the winning layer tag, read from the
/// engine, never re-derived; the exact `config_json` projection of
/// `crates/hekma/src/cli/agent.rs`).
fn render_leaves(effective: &hekma_engine::EffectiveConfig) -> Vec<dto::ConfigLeaf> {
    effective
        .iter()
        .map(|(key, resolved)| dto::ConfigLeaf {
            key: key.clone(),
            value: resolved.display(),
            source: resolved.source.as_str().to_string(),
            unvalidated: effective.is_unvalidated(key),
        })
        .collect()
}

/// `POST /v0/instances` — register a new instance from exactly one adapter
/// source. `201` ALWAYS on a committed registration, with the `Location:
/// /v0/instances/{name}` header (review item 13): the Fleet-row read-back is
/// the courtesy payload, but if a concurrent removal wins the race between
/// the commit and the read-back, the committed truth still answers `201`
/// (with an empty body — the client `GET`s the `Location`) rather than a
/// self-contradicting `404` for a request that SUCCEEDED.
pub(crate) async fn register(
    State(state): State<ApiState>,
    ApiJson(request): ApiJson<dto::RegisterRequest>,
) -> ApiResult<Response> {
    // Exactly-one-of, enforced HERE (the body shape's rule): both sources is
    // an ambiguous invocation, neither is no source — both 400s naming the
    // rule, the body-level mirror of the CLI's mutually-exclusive flags.
    let reference = match (request.kind.as_deref(), request.manifest.as_deref()) {
        (Some(_), Some(_)) => {
            return Err(ApiError::invalid_body(
                "exactly one adapter source is required: pass \"kind\" (a native kind) \
                 or \"manifest\" (an adapter.toml path), never both",
            ));
        }
        (None, None) => {
            return Err(ApiError::invalid_body(
                "exactly one adapter source is required: pass \"kind\" (a native kind) \
                 or \"manifest\" (an adapter.toml path)",
            ));
        }
        (Some(kind), None) => AdapterRef::Native(kind.to_string()),
        (None, Some(manifest)) => {
            // A RELATIVE manifest path would resolve against the HOST
            // process's working directory — an invisible dependency the API
            // contract does not define (review item 7): the request is
            // refused, naming the rule, instead of resolving by accident.
            if !std::path::Path::new(manifest).is_absolute() {
                return Err(ApiError::invalid_body(format!(
                    "manifest must be an ABSOLUTE path (a relative path would resolve \
                     against the host's working directory, which the API contract does \
                     not define): {manifest}"
                )));
            }
            AdapterRef::Manifest(std::path::PathBuf::from(manifest))
        }
    };
    state
        .engine
        .register_with_adapter(&request.name, &reference)
        .await?;
    // The registration COMMITTED — `201` is now the committed truth, whatever
    // the read-back says (review item 13).
    let location = format!("/v0/instances/{}", request.name);
    match state.engine.fleet_entry(&request.name).await {
        Ok(entry) => Ok((
            StatusCode::CREATED,
            [(header::LOCATION, location)],
            Json(dto::ShowDocument {
                schema_version: hekma_engine::FLEET_SCHEMA_VERSION,
                instance: entry,
            }),
        )
            .into_response()),
        Err(RegistryError::NotFound { .. }) => {
            // The race window: removed between the commit and this read. The
            // 201 + `Location` stand; the body is empty (nothing to show —
            // the client resolves the truth at the `Location`).
            Ok((StatusCode::CREATED, [(header::LOCATION, location)]).into_response())
        }
        Err(err) => Err(ApiError::from(err)),
    }
}

/// `DELETE /v0/instances/{name}` — remove the instance from the Fleet
/// (retaining its Agent Home, the CLI `remove` default; DELETE of a RUNNING
/// instance is a 409 naming the stop-first remediation — there is no force
/// flag on v0's surface).
pub(crate) async fn remove(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    state
        .engine
        .remove(&name, RemoveDisposition::Retain, false)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v0/instances/{name}/start` — the lifecycle verbs, each acknowledged
/// with the post-transition Fleet-row read-back (the state the instance
/// ACTUALLY reached, not the state the request hoped for).
pub(crate) async fn start(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<dto::ShowDocument>> {
    state.engine.start(&name).await?;
    read_back_show(&state, &name).await
}

/// `POST /v0/instances/{name}/pause`.
pub(crate) async fn pause(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<dto::ShowDocument>> {
    state.engine.pause(&name).await?;
    read_back_show(&state, &name).await
}

/// `POST /v0/instances/{name}/resume`.
pub(crate) async fn resume(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<dto::ShowDocument>> {
    state.engine.resume(&name).await?;
    read_back_show(&state, &name).await
}

/// `POST /v0/instances/{name}/stop` — the optional `window_secs` body bounds
/// the graceful ladder (the SAME `Engine::stop` call the host's SIGTERM
/// drain uses). A body that is PRESENT must parse — a malformed body is an
/// envelope `400`, never a silent "no body" (review item 10: silently
/// treating garbage as the engine default would hide the client's mistake
/// behind a stop that LOOKS fine). Only a truly ABSENT body (zero bytes)
/// selects the engine's default window.
pub(crate) async fn stop(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    body: Bytes,
) -> ApiResult<Json<dto::ShowDocument>> {
    let window = if body.is_empty() {
        // Absent body → the engine default.
        None
    } else {
        let request: dto::StopRequest = serde_json::from_slice(&body).map_err(|err| {
            ApiError::invalid_body(format!(
                "the stop body must be a JSON object like {{\"window_secs\":30}}, \
                 or no body at all for the engine's default window: {err}"
            ))
        })?;
        match request.window_secs {
            // `{}` / `{"window_secs":null}` → the default too.
            None => None,
            Some(secs) => {
                // The same bounds the `--stop-window` flag enforces (an
                // out-of-bounds window is a rejected REQUEST, not a
                // zero-second "graceful" kill).
                if !(crate::MIN_STOP_WINDOW_SECS..=crate::MAX_STOP_WINDOW_SECS).contains(&secs) {
                    return Err(ApiError::invalid_body(format!(
                        "window_secs must be a whole number of seconds from {} to {}",
                        crate::MIN_STOP_WINDOW_SECS,
                        crate::MAX_STOP_WINDOW_SECS
                    )));
                }
                Some(Duration::from_secs(secs))
            }
        }
    };
    state.engine.stop(&name, window).await?;
    read_back_show(&state, &name).await
}

/// The post-mutation read-back (`fleet_entry` → the show document): one
/// keyed O(1) read after a COMMITTED mutation. A read-back failure after the
/// engine already committed is an honest `500` (never a fabricated success
/// body) — the mutation itself stands, the acknowledgment failed.
async fn read_back_show(state: &ApiState, name: &str) -> ApiResult<Json<dto::ShowDocument>> {
    let entry = state.engine.fleet_entry(name).await?;
    Ok(Json(dto::ShowDocument {
        schema_version: hekma_engine::FLEET_SCHEMA_VERSION,
        instance: entry,
    }))
}

/// The `?cursor=` query parameter: a JSON-encoded
/// [`ResyncCursor`](hekma_engine::ResyncCursor) (`{"transitions":n,
/// "breaches":n,"usage":n}`); absent means [`ResyncCursor::START`] (the full
/// backfill — the crash-window heal's entry point).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct EventsQuery {
    /// The JSON-encoded per-family consume position.
    pub cursor: Option<String>,
}

/// `GET /v0/instances/{name}/events?cursor=` — the committed-truth pull:
/// `resync_events` verbatim (family-major events, the lossless next cursor,
/// `torn_tail_skipped` surfaced). A cursor past a family's committed count
/// is a typed 409 (`cursor_invalid`) — NEVER a clamp (the engine's guard,
/// mapped in [`super::error`]).
pub(crate) async fn events_pull(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Json<hekma_engine::ResyncBatch>> {
    let cursor = match query.cursor.as_deref() {
        None => ResyncCursor::START,
        Some(text) => serde_json::from_str(text).map_err(|err| {
            ApiError::invalid_body(format!(
                "the cursor query parameter must be a JSON-encoded resync cursor \
                 ({{\"transitions\":n,\"breaches\":n,\"usage\":n}}): {err}"
            ))
        })?,
    };
    let batch = state.engine.resync_events(&name, cursor).await?;
    Ok(Json(batch))
}
