//! The Control API's route handlers (story 17-3): thin projections of
//! `hekma-engine`'s PUBLIC async facade — every handler calls one engine
//! method and renders the mapped response, with error conversion delegated
//! entirely to the [`ApiError`](super::error::ApiError) mapper (`?`).
//!
//! Projection discipline (AD-20 / the frozen route table): only the named
//! facade methods appear here. Story 21-4 projects the setters the 17-3 table
//! had excluded "by design" because no CLI path needed them — every CLI write
//! now routes through this API (AD-20's one data-path owner): `send_input`,
//! the memory verbs, the restart-policy setter, `remove`'s disposition/force
//! and `start`'s detach. Still NOT projected: the log/raw-output reads (FR-41
//! forbids raw agent output unless explicitly enabled) and `reveal_secrets` —
//! NEVER, on any route, in any shape (the config read renders masked values
//! only).

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, RawQuery, State};
use axum::http::{header, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use hekma_engine::{
    AdapterRef, BreachAction, ChildRunRequest, ConfigLayer, FleetListing, LabelChange,
    RegistryError, RemoveDisposition, ResyncCursor, RunLimits, RunParent,
};
use serde::Deserialize;

use super::dto;
use super::error::{ApiError, ApiJson, ApiResult};
use super::ApiState;

/// `GET /v0/version` — the self-description (routes, auth, and stability
/// discovery all start here).
pub(crate) async fn version(State(state): State<ApiState>) -> Json<dto::VersionDocument> {
    // Story 17-4 (review finding 21): the capture posture rides the version
    // self-description — an off engine says so HERE, on the surface a Host
    // reads first, not only in a failed read round-trip. Story 21-4: read
    // at bind, so this probe never waits on an engine lock.
    let log_capture = state.log_capture;
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
            run: hekma_engine::RUN_EVENT_SCHEMA_VERSION,
        },
        log_capture: log_capture.as_str(),
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
    let live_capture = state.engine.has_live_capture(&name).await;
    Ok(Json(dto::StatusDocument {
        // ONE schema-version discipline for the whole 0.x API (review item
        // 11) — the status family carries the API-level stamp, no separate
        // counter.
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        status: status.into(),
        live_capture: Some(live_capture),
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

/// `POST /v0/instances/{name}/config/revealed` (story 22-2) — record that
/// the instance's custodied provider key is about to be revealed: one
/// `secret_revealed` event naming the secret, committed and published
/// before the answer. `hekma agent config get --reveal` calls it before it
/// prints a custodied key, and prints nothing when it fails. Operator
/// bearer only (the Operator route family).
pub(crate) async fn config_revealed(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<dto::RevealedDocument>> {
    let event = state.engine.note_secret_revealed(&name).await?;
    Ok(Json(dto::RevealedDocument {
        schema_version: hekma_engine::SECRET_REVEALED_SCHEMA_VERSION,
        secrets: event.map(|event| event.secrets).unwrap_or_default(),
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

/// The `DELETE /v0/instances/{name}` query (story 21-4): RAW strings parsed
/// in [`remove`] so every malformed value stays inside the versioned
/// envelope (the export `limit` discipline).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RemoveQuery {
    /// `retain` (the default — the Agent Home stays on disk) or `delete`.
    pub disposition: Option<String>,
    /// `true` removes even a running instance (the CLI's `--force`).
    pub force: Option<String>,
}

/// `DELETE /v0/instances/{name}?disposition=&force=` — remove the instance
/// from the Fleet. The default retains its Agent Home (the CLI `remove`
/// default); `disposition=delete` deletes it too. DELETE of a RUNNING
/// instance without `force=true` is a 409 naming the stop-first remediation.
pub(crate) async fn remove(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Query(query): Query<RemoveQuery>,
) -> ApiResult<StatusCode> {
    let disposition = match query.disposition.as_deref() {
        None | Some("retain") => RemoveDisposition::Retain,
        Some("delete") => RemoveDisposition::Delete,
        Some(other) => {
            return Err(ApiError::invalid_body(format!(
                "the disposition query parameter must be \"retain\" or \"delete\": {other}"
            )))
        }
    };
    let force = match query.force.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(other) => {
            return Err(ApiError::invalid_body(format!(
                "the force query parameter must be \"true\" or \"false\": {other}"
            )))
        }
    };
    state.engine.remove(&name, disposition, force).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v0/instances/{name}/start` — the lifecycle verbs, each acknowledged
/// with the post-transition Fleet-row read-back (the state the instance
/// ACTUALLY reached, not the state the request hoped for). Story 21-4: the
/// optional body `{"detach": true}` is the engine's detached start (the
/// handle is disarmed at spawn, so the agent also survives this host's exit);
/// an absent body is a supervised start. A PRESENT body must parse.
pub(crate) async fn start(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    body: Bytes,
) -> ApiResult<Json<dto::ShowDocument>> {
    let request = if body.is_empty() {
        dto::StartRequest::default()
    } else {
        serde_json::from_slice::<dto::StartRequest>(&body).map_err(|err| {
            ApiError::invalid_body(format!(
                "the start body must be a JSON object like {{\"detach\":true}}, or no \
                 body at all for a supervised start: {err}"
            ))
        })?
    };
    if request.detach {
        state.engine.start_detached(&name).await?;
    } else {
        state.engine.start(&name).await?;
    }
    read_back_show(&state, &name).await
}

/// `POST /v0/instances/{name}/send` (story 21-4) — deliver text to the
/// running agent's native input channel (`send_input`). `204` on delivery;
/// the host holds the pipe of every agent it started, so a send reaches an
/// agent another CLI command started.
pub(crate) async fn send(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    ApiJson(request): ApiJson<dto::SendRequest>,
) -> ApiResult<StatusCode> {
    state.engine.send_input(&name, &request.text).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /v0/instances/{name}/memory` (story 21-4) — attach a Memory Backing
/// (`{"kind":"filesystem"|"native"}`), answered with the managed directory
/// and the attachment read back. The attach is the committed truth: a
/// read-back failure after it still answers `200`, naming the failure in
/// `readback_error` (the backing REMAINS attached).
pub(crate) async fn memory_put(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    ApiJson(request): ApiJson<dto::MemoryPutRequest>,
) -> ApiResult<Json<dto::MemoryDocument>> {
    let kind = hekma_engine::MemoryBackingKind::from_wire(&request.kind).ok_or_else(|| {
        ApiError::invalid_body(format!(
            "unknown Memory Backing kind {:?}: this release accepts \"filesystem\" or \
             \"native\"",
            request.kind
        ))
    })?;
    let dir = state.engine.attach_memory(&name, kind).await?;
    let dir = dir.to_string_lossy().into_owned();
    // The attach committed; the read-back fills the attachment fields, or
    // names why it could not (the backing stays attached either way).
    let (status, readback_error) = match state.engine.memory_status(&name).await {
        Ok(Some(status)) => (Some(status), None),
        Ok(None) => (
            None,
            Some("the engine reports no attachment after the attach".to_string()),
        ),
        Err(err) => (None, Some(err.to_string())),
    };
    let document = dto::MemoryDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        instance: name,
        dir,
        kind: status.as_ref().map(|s| s.kind.as_str().to_string()),
        guarantee: status.as_ref().map(|s| s.guarantee.as_str().to_string()),
        declared: status.as_ref().map(|s| s.declared),
        readback_error,
    };
    Ok(Json(document))
}

/// `DELETE /v0/instances/{name}/memory` (story 21-4) — detach the Memory
/// Backing (metadata only: the managed directory stays on disk). `204`.
pub(crate) async fn memory_delete(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    state.engine.detach_memory(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /v0/instances/{name}/restart-policy` (story 21-4) — set the Restart
/// Policy (`{"policy":"never"|"on-failure"}`), acknowledged with the runtime
/// status read back (the policy it now carries).
pub(crate) async fn restart_policy_put(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    ApiJson(request): ApiJson<dto::RestartPolicyRequest>,
) -> ApiResult<Json<dto::StatusDocument>> {
    let policy = hekma_engine::RestartPolicy::from_wire(&request.policy).ok_or_else(|| {
        ApiError::invalid_body(format!(
            "unknown Restart Policy {:?}: this release accepts \"never\" or \"on-failure\"",
            request.policy
        ))
    })?;
    state.engine.set_restart_policy(&name, policy).await?;
    let status = state.engine.instance_status(&name).await?;
    Ok(Json(dto::StatusDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        status: status.into(),
        live_capture: None,
    }))
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

/// `POST /v0/instances/{name}/drain` (story 18-4) — the DRAIN composite:
/// pause via the adapter's declared `[lifecycle.pause]` COMMAND (cooperative,
/// no signal), wait — bounded — for the declared idle signal, then stop. The
/// optional `timeout_ms` body overrides the manifest tier (the three-tier
/// rule); out-of-bounds (zero, or past the 600_000 ceiling) is a rejected
/// REQUEST (400), the same shape as `stop`'s `window_secs` — never a
/// zero-millisecond "drain". BOTH outcomes are a `200` with the outcome
/// carried honestly in the body (`completed: false` = the bound burned
/// first; the stop still happened).
pub(crate) async fn drain(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    body: Bytes,
) -> ApiResult<Json<dto::DrainDocument>> {
    let bound = if body.is_empty() {
        // Absent body → the manifest tier (then the engine default).
        None
    } else {
        let request: dto::DrainRequest = serde_json::from_slice(&body).map_err(|err| {
            ApiError::invalid_body(format!(
                "the drain body must be a JSON object like {{\"timeout_ms\":1500}}, \
                 or no body at all for the manifest tier: {err}"
            ))
        })?;
        match request.timeout_ms {
            // `{}` / `{"timeout_ms":null}` → the manifest tier too.
            None => None,
            Some(ms) => {
                // The same 1..=600_000 ceiling the adapter contract pins for
                // the manifest tier (an out-of-bounds bound is a rejected
                // REQUEST, not a zero-timeout drain).
                if ms == 0 || ms > 600_000 {
                    return Err(ApiError::invalid_body(
                        "timeout_ms must be a whole number of milliseconds from 1 to 600000"
                            .to_string(),
                    ));
                }
                Some(Duration::from_millis(ms))
            }
        }
    };
    let report = state.engine.drain(&name, bound).await?;
    let entry = state.engine.fleet_entry(&name).await?;
    Ok(Json(dto::DrainDocument {
        schema_version: hekma_engine::FLEET_SCHEMA_VERSION,
        instance: entry,
        completed: report.outcome.completed,
        waited_ms: report.outcome.waited_ms,
    }))
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

/// The `?cursor=`/`?limit=` query parameters of the export pull (story
/// 17-4). `cursor` is a JSON-encoded [`OutboxCursor`](hekma_engine::OutboxCursor)
/// (`{"seq":n}`); ABSENT means "resume from the durable ACK cursor" — the
/// common give-me-whats-new pull (NOT `OutboxCursor::START`, which would
/// re-deliver the whole retained journal; a from-zero backfill is explicit:
/// `?cursor={"seq":0}`). `limit` bounds the batch (`None` = the engine's
/// default max). It is a RAW STRING parsed in [`export_pull`] (review
/// finding 16): a typed `Option<u32>` would let axum's `Query` extractor
/// reject a non-numeric or oversized value OUTSIDE the versioned envelope
/// (a plain-text rejection, not a `400` error document) — manual parsing
/// keeps EVERY query-parameter failure inside the envelope.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ExportQuery {
    /// The JSON-encoded export resume cursor.
    pub cursor: Option<String>,
    /// The batch-size bound (raw — parsed in [`export_pull`]).
    pub limit: Option<String>,
}

/// `GET /v0/export?cursor=&limit=` — the outbox pull (story 17-4): ONE
/// bounded batch of committed events in ONE global order (journal order ==
/// commit order), the engine's `export_events` verbatim. A cursor below the
/// journal's retained floor or past its committed tail is a typed 409
/// (`cursor_invalid`) — the engine's guard, mapped in [`super::error`] by
/// the stable refusal sentences.
pub(crate) async fn export_pull(
    State(state): State<ApiState>,
    Query(query): Query<ExportQuery>,
) -> ApiResult<Json<dto::ExportBody>> {
    let cursor = match query.cursor.as_deref() {
        None => None,
        Some(text) => Some(serde_json::from_str(text).map_err(|err| {
            ApiError::invalid_body(format!(
                "the cursor query parameter must be a JSON-encoded export cursor \
                 ({{\"seq\":n}}): {err}"
            ))
        })?),
    };
    // Review finding 16: the limit is parsed INSIDE the handler so every
    // failure is a versioned `400` envelope — never axum's plain-text
    // extractor rejection. `0` is refused outright: a zero batch is
    // meaningless, and silently delivering the default batch would lie
    // about the request (the ABSENT parameter, not `0`, means "default").
    let max = match query.limit.as_deref() {
        None => 0u32,
        Some(text) => match text.parse::<u32>() {
            Ok(0) => {
                return Err(ApiError::invalid_body(
                    "the limit query parameter must be >= 1 (omit it for the engine's \
                     default batch size)"
                        .to_string(),
                ))
            }
            Ok(n) => n,
            Err(err) => {
                return Err(ApiError::invalid_body(format!(
                    "the limit query parameter must be a positive whole number (u32): \
                     {err}"
                )))
            }
        },
    };
    let batch = state.engine.export_events(cursor, max).await?;
    Ok(Json(batch))
}

/// `POST /v0/export/ack` — advance the durable export ack cursor (story
/// 17-4): everything with `seq <= ack` is consumed and compaction-eligible.
/// Monotonic (`max(current, given)`); the response returns the cursor AS
/// PERSISTED so a re-ack reads back its own idempotence. An ack past the
/// journal's committed tail is a typed 409 (`cursor_invalid`).
pub(crate) async fn export_ack(
    State(state): State<ApiState>,
    body: axum::Json<dto::ExportAckRequest>,
) -> ApiResult<Json<dto::ExportAckDocument>> {
    let persisted = state.engine.ack_export(body.seq).await?;
    Ok(Json(dto::ExportAckDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        seq: persisted,
    }))
}

/// The `?selector=&view=&limit=&cursor=` query of the Run listing (story
/// 21-5). RAW strings parsed in [`runs_list`] (the export `limit`
/// discipline): every malformed value stays inside the versioned envelope.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RunsListQuery {
    /// `key=value[,key=value…]` — bound and caller labels alike.
    pub selector: Option<String>,
    /// `exceptions` (the default) or `all`.
    pub view: Option<String>,
    /// The page size (raw — parsed in [`runs_list`]; `0` = the maximum).
    pub limit: Option<String>,
    /// The cursor the previous page printed.
    pub cursor: Option<String>,
}

/// `GET /v0/runs?selector=&view=&limit=&cursor=` — the Run listing (story
/// 21-5, PRD FR-50): the engine's own document, byte-shape-equal to
/// `hekma ps --json`. Operator bearer (the auth layer); a malformed
/// Selector, limit or view is a `400` with its own machine code, a cursor
/// that is not the codec's form a `400 invalid-cursor` (the engine's typed
/// refusal), everything else the engine's mapped error.
pub(crate) async fn runs_list(
    State(state): State<ApiState>,
    Query(query): Query<RunsListQuery>,
) -> ApiResult<Json<hekma_engine::RunListingDocument>> {
    let view = match &query.view {
        None => hekma_engine::ListingView::Exceptions,
        Some(raw) => hekma_engine::ListingView::from_wire(raw).ok_or_else(|| {
            ApiError::invalid_query_param(
                "invalid-view",
                format!("the view query parameter must be \"exceptions\" or \"all\": {raw}"),
            )
        })?,
    };
    let selector = match &query.selector {
        None => None,
        Some(raw) => Some(raw.parse::<hekma_engine::Selector>().map_err(|err| {
            ApiError::invalid_query_param("invalid-selector", format!("selector: {err}"))
        })?),
    };
    // The limit parses INSIDE the handler so every failure is a versioned
    // envelope (never axum's plain-text extractor rejection). Absent, it is
    // the documented default page (50 — `hekma ps`'s default, so the two
    // first pages and cursors agree); `0` takes the maximum; anything above
    // the house page cap clamps to it (the CLI's `--limit` parity — its
    // clamp notes on stderr, this clamps quietly).
    let limit = match &query.limit {
        None => hekma_engine::LISTING_DEFAULT_LIMIT,
        Some(text) => text.parse::<usize>().map_err(|err| {
            ApiError::invalid_query_param(
                "invalid-limit",
                format!("the limit query parameter must be a whole number ≥ 0: {err}"),
            )
        })?,
    };
    let listing = state
        .engine
        .list_runs(hekma_engine::ListRunsQuery {
            view,
            selector: selector.clone(),
            after: query.cursor.clone(),
            limit,
        })
        .await?;
    Ok(Json(listing.into_document(view, selector)))
}

/// `POST /v0/runs` — open a child Run (story 21-2). `201` + `Location`
/// with the new Run; a retried request carrying the same `external_id`
/// answers `201` with the SAME Run (the open is idempotent by external id).
pub(crate) async fn run_open(
    State(state): State<ApiState>,
    Extension(authority): Extension<super::auth::Authority>,
    ApiJson(request): ApiJson<dto::RunOpenRequest>,
) -> ApiResult<Response> {
    let parent = match (request.parent_run_id, request.parent_instance) {
        (Some(run_id), None) => RunParent::Run(hekma_engine::RunId::from_wire(run_id)),
        (None, Some(instance)) => RunParent::Instance(instance),
        _ => {
            return Err(ApiError::invalid_body(
                "exactly one parent is required: pass \"parent_run_id\" (a Run) or \
                 \"parent_instance\" (an Agent Instance's open Run)",
            ))
        }
    };
    let breach_action = request
        .breach_action
        .as_deref()
        .map(str::parse::<BreachAction>)
        .transpose()
        .map_err(|err| ApiError::invalid_body(format!("breach_action: {err}")))?;
    let dollar_per_run_micros = request
        .dollar_cap
        .as_deref()
        .map(hekma_engine::parse_dollars_to_micros)
        .transpose()
        .map_err(|reason| ApiError::invalid_body(format!("dollar_cap: {reason}")))?;
    let opened = state
        .engine
        .open_child_run_as(
            authority.actor()?,
            ChildRunRequest {
                parent,
                labels: request.labels.into_iter().collect(),
                limits: RunLimits {
                    token_per_run: request.token_budget,
                    dollar_per_run_micros,
                    breach_action,
                    ..RunLimits::default()
                },
                external_id: request.external_id,
                issue_credentials: request.issue_credentials.unwrap_or(true),
            },
        )
        .await
        .map_err(|err| ApiError::for_authority(&authority, err))?;
    let location = format!("/v0/runs/{}", opened.view.record.run_id);
    let credentials = opened
        .credentials
        .as_ref()
        .map(dto::CredentialsDocument::from);
    let mut response = (
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(dto::RunOpenedDocument {
            schema_version: dto::CONTROL_API_SCHEMA_VERSION,
            run: opened.view,
            credentials,
        }),
    )
        .into_response();
    // A one-time credential must not sit in a cache.
    if opened.credentials.is_some() {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
    }
    Ok(response)
}

/// `GET /v0/runs/{id}` — one Run: its row, labels, limits, subtree usage
/// and open children.
pub(crate) async fn run_show(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> ApiResult<Json<dto::RunDocument>> {
    let view = state.engine.run(&run_id).await?;
    Ok(Json(dto::RunDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        run: view,
    }))
}

/// `POST /v0/runs/{id}/reconciliation` (story 19-5, Operator bearer only)
/// — submit the Run's confirmed usage. Answers the stored submission and
/// whether it was a replay of one already stored.
pub(crate) async fn run_reconcile(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    ApiJson(request): ApiJson<dto::ReconcileBody>,
) -> ApiResult<Json<dto::ReconcileDocument>> {
    let source = match request.source.as_deref() {
        None => hekma_engine::ReconcileSource::Host,
        Some(raw) => hekma_engine::ReconcileSource::from_wire(raw).ok_or_else(|| {
            ApiError::from(hekma_engine::RunError::Invalid {
                field: "source".to_string(),
                reason: format!("`{raw}` is not a source (provider or host)"),
            })
        })?,
    };
    let outcome = state
        .engine
        .reconcile_run(
            &run_id,
            hekma_engine::ReconcileRequest {
                reference: request.reference,
                source,
                input_tokens: request.input_tokens,
                output_tokens: request.output_tokens,
                cached_tokens: request.cached_tokens,
                cost_micros: request.cost_micros.map(hekma_engine::Micros),
                note: request.note,
            },
            hekma_engine::RunActor::Operator,
        )
        .await?;
    Ok(Json(dto::ReconcileDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        reconciliation: outcome.reconciliation,
        replayed: outcome.replayed,
    }))
}

/// `GET /v0/runs/{id}/reconciliation` (story 19-5, Operator bearer only)
/// — every submission for the Run, newest first.
pub(crate) async fn run_reconciliations(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> ApiResult<Json<dto::ReconciliationsDocument>> {
    let reconciliations = state.engine.run_reconciliations(&run_id).await?;
    Ok(Json(dto::ReconciliationsDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        reconciliations,
    }))
}

/// The `?run=&token_id=&outcome=&disposition=&status_class=&model=&depth=&
/// binding=&from=&to=&view=&limit=&cursor=` query of the calls listing (the
/// per-call read surface). RAW strings parsed in [`calls_list`] so every
/// malformed value stays inside the versioned envelope; the engine
/// validates the cursor, the run filter, the window and the closed-list
/// filters (one validation site for the API, the CLI and embedders).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct CallsListQuery {
    pub run: Option<String>,
    pub token_id: Option<String>,
    pub outcome: Option<String>,
    pub disposition: Option<String>,
    pub status_class: Option<String>,
    pub model: Option<String>,
    pub depth: Option<String>,
    pub binding: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// `all` (the default) or `problems`.
    pub view: Option<String>,
    /// The page size (raw — parsed in [`calls_list`]; `0` = the maximum).
    pub limit: Option<String>,
    /// The cursor the previous page printed.
    pub cursor: Option<String>,
}

/// `GET /v0/calls` — the per-call records listing: the engine's own
/// document, byte-shape-equal to `hekma calls list --json`. Operator
/// bearer (the auth layer — the calls routes sit in the Operator family,
/// and every other credential class answers `403 forbidden`). A `view`
/// that is neither `all` nor `problems` is a `400 invalid-view`; a limit
/// that is not a whole number is a `400 invalid-limit`; everything else
/// the engine refuses carries its own machine code.
pub(crate) async fn calls_list(
    State(state): State<ApiState>,
    Query(query): Query<CallsListQuery>,
) -> ApiResult<Json<hekma_engine::CallListingDocument>> {
    let view = match &query.view {
        None => hekma_engine::CallView::All,
        Some(raw) => hekma_engine::CallView::from_wire(raw).ok_or_else(|| {
            ApiError::invalid_query_param(
                "invalid-view",
                format!("the view query parameter must be \"all\" or \"problems\": {raw}"),
            )
        })?,
    };
    // The limit parses INSIDE the handler so every failure is a versioned
    // envelope (the export `limit` discipline). Absent, the documented
    // default page (50 — the CLI's default, so the two first pages agree);
    // `0` takes the maximum; anything above the house page cap clamps to
    // it quietly (the CLI's `--limit` parity — its clamp notes on stderr).
    let limit = match &query.limit {
        None => hekma_engine::CALLS_DEFAULT_LIMIT,
        Some(text) => text.parse::<usize>().map_err(|err| {
            ApiError::invalid_query_param(
                "invalid-limit",
                format!("the limit query parameter must be a whole number ≥ 0: {err}"),
            )
        })?,
    };
    let query = hekma_engine::CallsQuery {
        run: query.run,
        token_id: query.token_id,
        outcome: query.outcome,
        disposition: query.disposition,
        status_class: query.status_class,
        model: query.model,
        depth: query.depth,
        binding: query.binding,
        from: query.from,
        to: query.to,
        view,
        after: query.cursor,
        limit,
    };
    let filters = query.echoed_filters();
    let listing = state.engine.list_calls(query).await?;
    Ok(Json(listing.into_document(filters)))
}

/// `GET /v0/calls/{call_id}` — one per-call record. The id's shape is
/// checked here (`400 invalid-call-id`) so the refusal is a versioned
/// envelope; an unknown id is the engine's `404 call-not-found`.
pub(crate) async fn call_show(
    State(state): State<ApiState>,
    Path(call_id): Path<String>,
) -> ApiResult<Json<dto::CallDocument>> {
    if !hekma_engine::is_call_id(&call_id) {
        return Err(ApiError::invalid_query_param(
            "invalid-call-id",
            format!("{call_id} is not a call id this engine records (call_ plus 26 characters)"),
        ));
    }
    let record = state.engine.call(&call_id).await?;
    Ok(Json(dto::CallDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        call: record,
    }))
}

/// `POST /v0/runs/{id}/close` — close a child Run (idempotent; a Run that
/// owns a process is a `409` naming the stop remediation).
pub(crate) async fn run_close(
    State(state): State<ApiState>,
    Extension(authority): Extension<super::auth::Authority>,
    Path(run_id): Path<String>,
) -> ApiResult<Json<dto::RunDocument>> {
    let view = state
        .engine
        .close_run_by(authority.lease_actor()?, &run_id)
        .await
        .map_err(|err| ApiError::for_authority(&authority, err))?;
    Ok(Json(dto::RunDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        run: view,
    }))
}

/// `POST /v0/runs/announce` (story 23-2) — announce a Run beneath the
/// presented announce credential's binding. `201` with the Run and its
/// custody-eligible Run Key and refresh credential, shown once, when the
/// identity was free (or its Run had ended); `200` with the renewed Run and
/// no credentials when it was open. Only an announce credential reaches
/// this handler (the matrix).
pub(crate) async fn run_announce(
    State(state): State<ApiState>,
    Extension(authority): Extension<super::auth::Authority>,
    ApiJson(request): ApiJson<dto::AnnounceRequest>,
) -> ApiResult<Response> {
    let super::auth::Authority::Announcer(credential) = &authority else {
        return Err(ApiError::forbidden());
    };
    let announced = state
        .engine
        .announce(
            credential.clone(),
            &request.external_id,
            request.labels.into_iter().collect(),
        )
        .await
        .map_err(|err| ApiError::for_authority(&authority, err))?;
    let location = format!("/v0/runs/{}", announced.view.record.run_id);
    let credentials = announced
        .credentials
        .as_ref()
        .map(dto::CredentialsDocument::from);
    let (status, note) = if announced.created {
        (StatusCode::CREATED, dto::ANNOUNCE_CREATED_NOTE)
    } else {
        (StatusCode::OK, dto::ANNOUNCE_RENEWED_NOTE)
    };
    let mut response = (
        status,
        [(header::LOCATION, location)],
        Json(dto::AnnouncedDocument {
            schema_version: dto::CONTROL_API_SCHEMA_VERSION,
            run: announced.view,
            created: announced.created,
            credentials,
            note,
        }),
    )
        .into_response();
    // A one-time credential must not sit in a cache.
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

/// `POST /v0/runs/{id}/heartbeat` (story 23-2) — renew the Run's Lease and
/// bring a `silent` Run back to `active`. A Run Key within its own subtree,
/// or the announce credential that created the Run. `204`; an ended Run is
/// `409 run-ended`.
pub(crate) async fn run_heartbeat(
    State(state): State<ApiState>,
    Extension(authority): Extension<super::auth::Authority>,
    Path(run_id): Path<String>,
) -> ApiResult<StatusCode> {
    match state
        .engine
        .heartbeat(authority.lease_actor()?, &run_id)
        .await
    {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(err @ hekma_engine::RunError::RunEnded { .. }) => Err(ApiError::from(err)
            .with_message(format!(
                "Run `{run_id}` has ended, so it has no Lease to renew; announce again to start \
                 a new Run"
            ))),
        Err(err) => Err(ApiError::for_authority(&authority, err)),
    }
}

/// One hold-family verb on one Run (story 23-4).
async fn disposition_one(
    state: &ApiState,
    run_id: &str,
    verb: hekma_engine::DispositionVerb,
) -> ApiResult<dto::DispositionDocument> {
    let outcome = state
        .engine
        .set_disposition(run_id, verb, "operator")
        .await?;
    Ok(dto::DispositionDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        run_id: run_id.to_string(),
        changed: outcome.changed,
        reasons: outcome.reasons,
        note: dto::DISPOSITION_NOTE,
    })
}

/// `POST /v0/runs/{id}/hold` (story 23-4, Operator only) — hold the Run's
/// model calls at the metering proxy.
pub(crate) async fn run_hold(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> ApiResult<Json<dto::DispositionDocument>> {
    Ok(Json(
        disposition_one(&state, &run_id, hekma_engine::DispositionVerb::Hold).await?,
    ))
}

/// `POST /v0/runs/{id}/release` (story 23-4) — release the hold.
pub(crate) async fn run_release(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> ApiResult<Json<dto::DispositionDocument>> {
    Ok(Json(
        disposition_one(&state, &run_id, hekma_engine::DispositionVerb::Release).await?,
    ))
}

/// `POST /v0/runs/{id}/reject` (story 23-4) — refuse the Run's calls.
pub(crate) async fn run_reject(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> ApiResult<Json<dto::DispositionDocument>> {
    Ok(Json(
        disposition_one(&state, &run_id, hekma_engine::DispositionVerb::Reject).await?,
    ))
}

/// `POST /v0/runs/{id}/reroute` (story 23-4) — `{"route": "<binding>/<route>"}`
/// sends the Run's calls to the route; `{"route": null}` sends them back.
pub(crate) async fn run_reroute(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    ApiJson(request): ApiJson<dto::RerouteRequest>,
) -> ApiResult<Json<dto::DispositionDocument>> {
    Ok(Json(
        disposition_one(
            &state,
            &run_id,
            hekma_engine::DispositionVerb::Reroute(request.route),
        )
        .await?,
    ))
}

/// The hold-family set forms (story 23-4): one result per Run.
async fn set_disposition(
    state: ApiState,
    credential: super::auth::Credential,
    name: &'static str,
    verb: impl Fn(Option<String>) -> hekma_engine::DispositionVerb,
    request: dto::SetDispositionRequest,
) -> ApiResult<Json<dto::ActResultsDocument>> {
    if name != "reroute" && request.route.is_some() {
        return Err(ApiError::invalid_body(format!(
            "route applies to reroute only, not {name}"
        )));
    }
    let run_ids = consume_set(&state, credential, &request.set_token)?;
    let mut results = Vec::with_capacity(run_ids.len());
    for run_id in run_ids {
        results.push(
            match state
                .engine
                .set_disposition(&run_id, verb(request.route.clone()), "operator")
                .await
            {
                Ok(outcome) => dto::ActResult {
                    run_id,
                    outcome: if outcome.changed { name } else { "unchanged" }.to_string(),
                    ..dto::ActResult::default()
                },
                Err(error) => refused(run_id, error.into()),
            },
        );
    }
    Ok(Json(dto::ActResultsDocument {
        schema_version: dto::ACT_RESULTS_SCHEMA_VERSION,
        dry_run: false,
        verb: name,
        matched: results.len(),
        results,
    }))
}

/// `POST /v0/runs/hold` — hold every Run of a selected set.
pub(crate) async fn runs_hold(
    State(state): State<ApiState>,
    axum::Extension(credential): axum::Extension<super::auth::Credential>,
    ApiJson(request): ApiJson<dto::SetDispositionRequest>,
) -> ApiResult<Json<dto::ActResultsDocument>> {
    set_disposition(
        state,
        credential,
        "hold",
        |_| hekma_engine::DispositionVerb::Hold,
        request,
    )
    .await
}

/// `POST /v0/runs/release` — release every Run of a selected set.
pub(crate) async fn runs_release(
    State(state): State<ApiState>,
    axum::Extension(credential): axum::Extension<super::auth::Credential>,
    ApiJson(request): ApiJson<dto::SetDispositionRequest>,
) -> ApiResult<Json<dto::ActResultsDocument>> {
    set_disposition(
        state,
        credential,
        "release",
        |_| hekma_engine::DispositionVerb::Release,
        request,
    )
    .await
}

/// `POST /v0/runs/reject` — reject every Run of a selected set.
pub(crate) async fn runs_reject(
    State(state): State<ApiState>,
    axum::Extension(credential): axum::Extension<super::auth::Credential>,
    ApiJson(request): ApiJson<dto::SetDispositionRequest>,
) -> ApiResult<Json<dto::ActResultsDocument>> {
    set_disposition(
        state,
        credential,
        "reject",
        |_| hekma_engine::DispositionVerb::Reject,
        request,
    )
    .await
}

/// `POST /v0/runs/reroute` — reroute every Run of a selected set.
pub(crate) async fn runs_reroute(
    State(state): State<ApiState>,
    axum::Extension(credential): axum::Extension<super::auth::Credential>,
    ApiJson(request): ApiJson<dto::SetDispositionRequest>,
) -> ApiResult<Json<dto::ActResultsDocument>> {
    set_disposition(
        state,
        credential,
        "reroute",
        hekma_engine::DispositionVerb::Reroute,
        request,
    )
    .await
}

/// `POST /v0/runs/refresh` (story 22-1) — trade the presented refresh
/// credential for a new Run Key and refresh credential, once. Only a
/// refresh credential reaches this handler (the matrix).
pub(crate) async fn run_refresh(
    State(state): State<ApiState>,
    Extension(authority): Extension<super::auth::Authority>,
) -> ApiResult<Response> {
    let super::auth::Authority::Refresh(presented) = &authority else {
        return Err(ApiError::forbidden());
    };
    // Never the Operator on this route (the matrix), so a store failure is
    // always the fixed `store` body, never the backend's text.
    let issued = state
        .engine
        .refresh_credentials(presented)
        .await
        .map_err(|err| ApiError::for_authority(&authority, err))?;
    let mut response = Json(dto::RefreshDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        run_id: issued.run_id().clone(),
        credentials: dto::CredentialsDocument::from(&issued),
    })
    .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

/// `POST /v0/runs/{id}/revoke-keys` (story 22-1, Operator only) — revoke
/// the Run's active credentials and, by default, every credential issued
/// under them. Committed and published before this answers.
pub(crate) async fn run_revoke_keys(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<dto::RevokeKeysDocument>> {
    let request: dto::RevokeKeysRequest = if body.iter().all(u8::is_ascii_whitespace) {
        dto::RevokeKeysRequest::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|err| ApiError::invalid_body(format!("invalid revoke-keys body: {err}")))?
    };
    let outcome = state
        .engine
        .revoke_run_keys(&run_id, request.cascade.unwrap_or(true))
        .await?;
    Ok(Json(dto::RevokeKeysDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        outcome,
    }))
}

/// `GET /v0/runs/{id}/keys` (story 22-1, Operator only) — a Run's
/// credential metadata: token ids, classes, states, never a value.
pub(crate) async fn run_keys(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> ApiResult<Json<dto::RunKeysDocument>> {
    let keys = state.engine.run_keys(&run_id).await?;
    Ok(Json(dto::RunKeysDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        run_id: hekma_engine::RunId::from_wire(run_id),
        keys,
    }))
}

/// `GET /v0/runs/{id}/labels` — one Run's labels (story 21-9), bound and
/// Caller, each with its source and (bound) origin.
pub(crate) async fn run_labels_get(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> ApiResult<Json<dto::RunLabelsDocument>> {
    let view = state.engine.run(&run_id).await?;
    Ok(Json(dto::RunLabelsDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        run_id: view.record.run_id,
        labels: view.labels,
    }))
}

/// `PATCH /v0/runs/{id}/labels` — bind and unbind labels on one Run as the
/// Operator (story 21-9). `200` with the labels after (or as planned on a
/// dry run); a refusal is `404 run-not-found` or `409` (`run-ended`,
/// `label-from-config`, `label-not-bound`), and a malformed change is
/// `400 invalid_body`.
pub(crate) async fn run_labels_patch(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    ApiJson(request): ApiJson<dto::RunLabelsPatch>,
) -> ApiResult<Json<dto::RunLabelsChangeDocument>> {
    let change = LabelChange {
        set: request.set,
        remove: request.remove,
    };
    let outcome = state
        .engine
        .label_run(&run_id, change, request.dry_run)
        .await?;
    Ok(Json(dto::RunLabelsChangeDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        run_id: outcome.run_id,
        labels: outcome.labels,
        prior_labels: outcome.prior_labels,
        changed: outcome.changed,
        dry_run: outcome.dry_run,
    }))
}

/// One change answer from an engine outcome.
fn policy_change_document(
    outcome: hekma_engine::PolicyOutcome,
    dry_run: bool,
) -> dto::PolicyChangeDocument {
    dto::PolicyChangeDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        policy: outcome.record,
        matched: outcome.matched,
        fallbacks: outcome.fallbacks,
        notes: outcome.notes,
        dry_run,
    }
}

/// `GET /v0/policies` — every stored Policy (story 21-3), `default`
/// included, by name.
pub(crate) async fn policies_list(
    State(state): State<ApiState>,
) -> ApiResult<Json<dto::PolicyListDocument>> {
    let policies = state.engine.policies().await?;
    Ok(Json(dto::PolicyListDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        policies,
    }))
}

/// `GET /v0/policies/{name}` — one Policy; `404 policy-not-found` when no
/// Policy has the name.
pub(crate) async fn policy_show(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<dto::PolicyShowDocument>> {
    let policy = state.engine.policy(&name).await?;
    Ok(Json(dto::PolicyShowDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        policy,
    }))
}

/// `POST /v0/policies` — create a Policy (story 21-3). `201` with a
/// `Location` and the matched and fallback counts (`200` on a dry run);
/// `409 policy-exists` when the name is taken; a malformed request is
/// `400 invalid_body`.
pub(crate) async fn policy_create(
    State(state): State<ApiState>,
    ApiJson(request): ApiJson<dto::PolicyCreate>,
) -> ApiResult<Response> {
    let dry_run = request.dry_run;
    let change = hekma_engine::PolicyChange {
        name: request.name.clone(),
        selector: Some(request.selector),
        set: hekma_engine::PolicyLimits {
            tokens_per_run: request.tokens_per_run,
            tokens_cumulative: request.tokens_cumulative,
            dollars_per_run_micros: request.dollars_per_run_micros,
            dollars_cumulative_micros: request.dollars_cumulative_micros,
            breach_action: request.breach_action,
            scent_check: request.scent_check,
            rate: request.rate.map(dto::RatePatch::into_rate),
        },
        clear: Vec::new(),
        rates: std::collections::BTreeMap::new(),
    };
    let outcome = state.engine.create_policy(change, dry_run).await?;
    let document = Json(policy_change_document(outcome, dry_run));
    if dry_run {
        return Ok(document.into_response());
    }
    let location = format!("/v0/policies/{}", request.name);
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        document,
    )
        .into_response())
}

/// `PATCH /v0/policies/{name}` — change a Policy, the `default` Policy
/// included (story 21-3). `200` with the Policy after (or as planned) and
/// the matched and fallback counts; `404 policy-not-found`; a malformed
/// change is `400 invalid_body`.
pub(crate) async fn policy_change(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    ApiJson(request): ApiJson<dto::PolicyPatch>,
) -> ApiResult<Json<dto::PolicyChangeDocument>> {
    let dry_run = request.dry_run;
    // A model named in BOTH `set` and `remove` is a contradiction, not a
    // silent remove (the remove once won the map insert and the priced
    // entry quietly disappeared).
    for model in &request.rates.remove {
        if request.rates.set.contains_key(model) {
            return Err(ApiError::invalid_body(format!(
                "the model `{model}` is named in both rates.set and rates.remove — pick one"
            )));
        }
    }
    let mut rates: std::collections::BTreeMap<String, Option<hekma_engine::Rate>> = request
        .rates
        .set
        .into_iter()
        .map(|(model, rate)| (model, Some(rate.into_rate())))
        .collect();
    for model in request.rates.remove {
        rates.insert(model, None);
    }
    let change = hekma_engine::PolicyChange {
        name,
        selector: request.selector,
        set: request.set.into_limits(),
        clear: request.clear,
        rates,
    };
    let outcome = state.engine.change_policy(change, dry_run).await?;
    Ok(Json(policy_change_document(outcome, dry_run)))
}

/// `DELETE /v0/policies/{name}` — delete a Policy (story 21-3). `204`
/// (`200` with the counts on `?dry_run=true`); `404 policy-not-found`;
/// deleting `default` is `409 policy-default`.
pub(crate) async fn policy_delete(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    Query(query): Query<dto::PolicyDeleteQuery>,
) -> ApiResult<Response> {
    let outcome = state.engine.delete_policy(&name, query.dry_run).await?;
    if query.dry_run {
        return Ok(Json(policy_change_document(outcome, true)).into_response());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Story 21-6: Selector sets, the Run-grain verbs and Run-level limits.
// ---------------------------------------------------------------------------

/// A `window_secs` value inside the host's stop-window bounds, or the
/// request refusal.
fn checked_window(window_secs: Option<u64>) -> ApiResult<Option<Duration>> {
    match window_secs {
        None => Ok(None),
        Some(secs) => {
            if !(crate::MIN_STOP_WINDOW_SECS..=crate::MAX_STOP_WINDOW_SECS).contains(&secs) {
                return Err(ApiError::invalid_body(format!(
                    "window_secs must be a whole number of seconds from {} to {}",
                    crate::MIN_STOP_WINDOW_SECS,
                    crate::MAX_STOP_WINDOW_SECS
                )));
            }
            Ok(Some(Duration::from_secs(secs)))
        }
    }
}

/// A `timeout_ms` value inside the drain bound, or the request refusal.
fn checked_drain_bound(timeout_ms: Option<u64>) -> ApiResult<Option<Duration>> {
    match timeout_ms {
        None => Ok(None),
        Some(ms) if ms == 0 || ms > 600_000 => Err(ApiError::invalid_body(
            "timeout_ms must be a whole number of milliseconds from 1 to 600000".to_string(),
        )),
        Some(ms) => Ok(Some(Duration::from_millis(ms))),
    }
}

/// An optional JSON body: absent (zero bytes) is the default, a present one
/// must parse.
fn optional_body<T: serde::de::DeserializeOwned + Default>(
    body: &Bytes,
    shape: &str,
) -> ApiResult<T> {
    if body.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|err| {
        ApiError::invalid_body(format!(
            "the body must be a JSON object like {shape}, or no body at all: {err}"
        ))
    })
}

/// `POST /v0/runs/select` — resolve a Selector into a set to act on (story
/// 21-6, AD-20's set token): every non-ended Run carrying all its terms,
/// at most `MAX_SELECTED_RUNS` (more is `400 too-many-runs`, never cut).
/// The answer's token is single-use, bound to this credential and expires
/// after 10 minutes; a set-form verb acts on exactly the listed Runs.
pub(crate) async fn runs_select(
    State(state): State<ApiState>,
    axum::Extension(credential): axum::Extension<super::auth::Credential>,
    ApiJson(request): ApiJson<dto::SelectRequest>,
) -> ApiResult<Json<dto::SelectDocument>> {
    let selector = hekma_engine::Selector::parse(&request.selector)
        .map_err(|reason| ApiError::invalid_body(format!("selector: {reason}")))?;
    let runs = state.engine.select_run_summaries(&selector).await?;
    let minted = state
        .sets
        .mint(
            credential.0,
            runs.iter()
                .map(|run| run.record.run_id.to_string())
                .collect(),
        )
        .map_err(|reason| {
            ApiError::new(
                super::error::ApiErrorKind::Internal,
                format!("could not mint a set token: {reason}"),
            )
        })?;
    Ok(Json(dto::SelectDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        token: minted.token,
        matched: runs.len(),
        runs,
        expires_at: minted.expires_at,
    }))
}

/// One Run-grain lifecycle verb on one Run (the per-Run routes and each
/// member of a set act share it).
async fn act_one(
    state: &ApiState,
    verb: hekma_engine::RunLifecycleVerb,
    run_id: &str,
    window: Option<Duration>,
    bound: Option<Duration>,
) -> ApiResult<dto::RunActDocument> {
    use hekma_engine::RunLifecycleVerb as Verb;
    // Story 23-4: on a wire Run the verb acts on its model calls at the
    // metering proxy — pause holds them, resume releases them, stop and
    // drain reject them and end the Run.
    let record = state.engine.plan_run_verb(run_id, verb).await?;
    if let Some(disposition) = hekma_engine::disposition_for(verb, &record) {
        state
            .engine
            .set_disposition(run_id, disposition, "operator")
            .await?;
        if matches!(verb, Verb::Stop | Verb::Drain) {
            state.engine.close_run(run_id).await?;
        }
        return Ok(dto::RunActDocument {
            schema_version: dto::CONTROL_API_SCHEMA_VERSION,
            run_id: run_id.to_string(),
            instance: None,
            completed: None,
            waited_ms: None,
        });
    }
    let (instance, drain) = match verb {
        Verb::Stop => (state.engine.stop_run(run_id, window).await?, None),
        Verb::Pause => (state.engine.pause_run(run_id).await?, None),
        Verb::Resume => (state.engine.resume_run(run_id).await?, None),
        Verb::Drain => {
            let report = state.engine.drain_run(run_id, bound).await?;
            (report.instance, Some(report.outcome))
        }
    };
    let read_back = state
        .engine
        .fleet_entry(instance.name.as_str())
        .await
        .map_err(ApiError::from);
    Ok(acted(
        run_id,
        read_back,
        drain.map(|outcome| (outcome.completed, outcome.waited_ms)),
    ))
}

/// The answer for a verb that COMMITTED on `run_id`: its Fleet row read
/// back, when the read succeeded. A failed read-back drops only the row —
/// the Run acted, so it is never reported as refused.
fn acted(
    run_id: &str,
    read_back: ApiResult<hekma_engine::FleetEntry>,
    drain: Option<(bool, u64)>,
) -> dto::RunActDocument {
    dto::RunActDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        run_id: run_id.to_string(),
        instance: read_back.ok(),
        completed: drain.map(|(completed, _)| completed),
        waited_ms: drain.map(|(_, waited_ms)| waited_ms),
    }
}

/// `POST /v0/runs/{id}/stop` — stop the instance whose process the Run
/// owns (optional `{"window_secs": n}`); a child Run, an ended Run and any
/// depth the verb × depth table does not serve are refused (`409`).
pub(crate) async fn run_stop(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<dto::RunActDocument>> {
    let request: dto::StopRequest = optional_body(&body, "{\"window_secs\":30}")?;
    let window = checked_window(request.window_secs)?;
    Ok(Json(
        act_one(
            &state,
            hekma_engine::RunLifecycleVerb::Stop,
            &run_id,
            window,
            None,
        )
        .await?,
    ))
}

/// `POST /v0/runs/{id}/pause`.
pub(crate) async fn run_pause(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> ApiResult<Json<dto::RunActDocument>> {
    Ok(Json(
        act_one(
            &state,
            hekma_engine::RunLifecycleVerb::Pause,
            &run_id,
            None,
            None,
        )
        .await?,
    ))
}

/// `POST /v0/runs/{id}/resume`.
pub(crate) async fn run_resume(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> ApiResult<Json<dto::RunActDocument>> {
    Ok(Json(
        act_one(
            &state,
            hekma_engine::RunLifecycleVerb::Resume,
            &run_id,
            None,
            None,
        )
        .await?,
    ))
}

/// `POST /v0/runs/{id}/drain` (optional `{"timeout_ms": n}`).
pub(crate) async fn run_drain(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<dto::RunActDocument>> {
    let request: dto::DrainRequest = optional_body(&body, "{\"timeout_ms\":1500}")?;
    let bound = checked_drain_bound(request.timeout_ms)?;
    Ok(Json(
        act_one(
            &state,
            hekma_engine::RunLifecycleVerb::Drain,
            &run_id,
            None,
            bound,
        )
        .await?,
    ))
}

/// Build the engine change from the limit fields (no engine needed).
fn limit_change(patch: &dto::RunLimitsPatch) -> ApiResult<hekma_engine::RunLimitChange> {
    let dollars = |field: &str, raw: &Option<String>| {
        raw.as_deref()
            .map(hekma_engine::parse_dollars_to_micros)
            .transpose()
            .map_err(|reason| ApiError::invalid_body(format!("{field}: {reason}")))
    };
    let breach_action = patch
        .breach_action
        .as_deref()
        .map(str::parse::<BreachAction>)
        .transpose()
        .map_err(|err| ApiError::invalid_body(format!("breach_action: {err}")))?;
    let scent_check = patch
        .scent_check
        .as_deref()
        .map(|raw| {
            hekma_engine::ScentCheckMode::from_wire(raw).ok_or_else(|| {
                ApiError::invalid_body(format!(
                    "scent_check: `{raw}` is not a Scent-Check Mode — use off, shadow or enforce"
                ))
            })
        })
        .transpose()?;
    let mut clear = Vec::with_capacity(patch.clear.len());
    for raw in &patch.clear {
        let field = hekma_engine::RunLimitField::from_wire(raw).ok_or_else(|| {
            ApiError::invalid_body(format!(
                "clear: `{raw}` is not a limit — use tokens-per-run, tokens-cumulative, \
                 dollars-per-run, dollars-cumulative or breach-action"
            ))
        })?;
        if !clear.contains(&field) {
            clear.push(field);
        }
    }
    let change = hekma_engine::RunLimitChange {
        set: RunLimits {
            token_per_run: patch.tokens_per_run,
            dollar_per_run_micros: dollars("dollars_per_run", &patch.dollars_per_run)?,
            token_cumulative: patch.tokens_cumulative,
            dollar_cumulative_micros: dollars("dollars_cumulative", &patch.dollars_cumulative)?,
            breach_action,
            scent_check,
        },
        clear,
    };
    change.validate()?;
    Ok(change)
}

/// `GET /v0/runs/{id}/limits` — one Run's budget read: the limits set on
/// the Run, the resolved limits with their sources, the subtree and
/// cumulative usage, the headroom and the latches.
pub(crate) async fn run_limits_get(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> ApiResult<Json<dto::RunLimitsDocument>> {
    let view = state.engine.run_limits_view(&run_id).await?;
    Ok(Json(dto::RunLimitsDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        view,
    }))
}

/// `PATCH /v0/runs/{id}/limits` — set or clear limits on one Run as the
/// Operator (this bearer is the Operator's; the Caller credential classes
/// that may only tighten arrive with their own surfaces). A limit set here
/// belongs to the Run (`run_scoped: true`). Refusals: `404 run-not-found`,
/// `409 run-ended`, `400 scope-unavailable` / `action-unavailable` /
/// `invalid-request`.
pub(crate) async fn run_limits_patch(
    State(state): State<ApiState>,
    Extension(authority): Extension<super::auth::Authority>,
    Path(run_id): Path<String>,
    ApiJson(request): ApiJson<dto::RunLimitsPatch>,
) -> ApiResult<Json<dto::RunLimitsChangeDocument>> {
    let change = limit_change(&request)?;
    // Story 22-1: the operator bearer acts as the Operator; a Run Key as
    // its Run's Caller (tighten-only, within its subtree, re-validated in
    // the job).
    let mut results = state
        .engine
        .set_run_limits_as(vec![run_id], change, authority.actor()?, request.dry_run)
        .await
        .map_err(|err| ApiError::for_authority(&authority, err))?;
    let (_, outcome) = results.pop().ok_or_else(|| {
        ApiError::new(
            super::error::ApiErrorKind::Internal,
            "the budget job answered no Run",
        )
    })?;
    Ok(Json(dto::RunLimitsChangeDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        outcome: outcome.map_err(|err| ApiError::for_authority(&authority, err))?,
    }))
}

/// Consume a set token for this request's credential.
fn consume_set(
    state: &ApiState,
    credential: super::auth::Credential,
    token: &str,
) -> ApiResult<Vec<String>> {
    state
        .sets
        .consume(token, credential.0)
        .map_err(ApiError::set_token)
}

/// A refused Run's result row.
fn refused(run_id: String, error: ApiError) -> dto::ActResult {
    dto::ActResult {
        run_id,
        outcome: "refused".to_string(),
        error: Some(error.error_object()),
        ..dto::ActResult::default()
    }
}

/// The past tense a lifecycle verb's result carries.
fn past_tense(verb: hekma_engine::RunLifecycleVerb) -> &'static str {
    use hekma_engine::RunLifecycleVerb as Verb;
    match verb {
        Verb::Stop => "stopped",
        Verb::Pause => "paused",
        Verb::Resume => "resumed",
        Verb::Drain => "drained",
    }
}

/// A set-form lifecycle verb: consume the token, act on EXACTLY the listed
/// Runs — the per-Run calls issued concurrently and joined (the engine
/// serializes the ladders under its locks, so a set's worst case is the sum
/// of the windows) — and answer one result per Run, in set order. A Run
/// that ended since the select is that Run's `run-ended` refusal; the rest
/// act.
async fn set_act(
    state: ApiState,
    credential: super::auth::Credential,
    verb: hekma_engine::RunLifecycleVerb,
    request: dto::SetActRequest,
) -> ApiResult<Json<dto::ActResultsDocument>> {
    use hekma_engine::RunLifecycleVerb as Verb;
    let window = match verb {
        Verb::Stop => checked_window(request.window_secs)?,
        _ if request.window_secs.is_some() => {
            return Err(ApiError::invalid_body(format!(
                "window_secs applies to stop only, not {}",
                verb.as_str()
            )))
        }
        _ => None,
    };
    let bound = match verb {
        Verb::Drain => checked_drain_bound(request.timeout_ms)?,
        _ if request.timeout_ms.is_some() => {
            return Err(ApiError::invalid_body(format!(
                "timeout_ms applies to drain only, not {}",
                verb.as_str()
            )))
        }
        _ => None,
    };
    let run_ids = consume_set(&state, credential, &request.set_token)?;
    // Every Run's own refusal is decided FIRST, against the set as it
    // stands when the act begins — so a child is answered from its own
    // cell, never from what its parent's concurrent stop then did to it.
    let mut planned = Vec::with_capacity(run_ids.len());
    for run_id in &run_ids {
        planned.push(state.engine.plan_run_verb(run_id, verb).await.err());
    }
    let mut tasks = Vec::with_capacity(run_ids.len());
    for (run_id, refusal) in run_ids.iter().cloned().zip(planned) {
        let state = state.clone();
        tasks.push(tokio::spawn(async move {
            let acted = match refusal {
                Some(refusal) => Err(ApiError::from(refusal)),
                None => act_one(&state, verb, &run_id, window, bound).await,
            };
            (run_id, acted)
        }));
    }
    let mut results = Vec::with_capacity(tasks.len());
    for (task, run_id) in tasks.into_iter().zip(run_ids.iter()) {
        results.push(match task.await {
            Ok((run_id, Ok(document))) => dto::ActResult {
                run_id,
                outcome: past_tense(verb).to_string(),
                instance: document.instance,
                completed: document.completed,
                waited_ms: document.waited_ms,
                ..dto::ActResult::default()
            },
            Ok((run_id, Err(error))) => refused(run_id, error),
            Err(join) => refused(
                run_id.clone(),
                ApiError::new(
                    super::error::ApiErrorKind::Internal,
                    format!("the act on this Run did not complete: {join}"),
                ),
            ),
        });
    }
    Ok(Json(dto::ActResultsDocument {
        schema_version: dto::ACT_RESULTS_SCHEMA_VERSION,
        dry_run: false,
        verb: verb.as_str(),
        matched: results.len(),
        results,
    }))
}

/// `POST /v0/runs/stop` — stop every Run of a selected set (`{"set_token":
/// …, "window_secs"?: n}`).
pub(crate) async fn runs_stop(
    State(state): State<ApiState>,
    axum::Extension(credential): axum::Extension<super::auth::Credential>,
    ApiJson(request): ApiJson<dto::SetActRequest>,
) -> ApiResult<Json<dto::ActResultsDocument>> {
    set_act(
        state,
        credential,
        hekma_engine::RunLifecycleVerb::Stop,
        request,
    )
    .await
}

/// `POST /v0/runs/pause` — pause every Run of a selected set.
pub(crate) async fn runs_pause(
    State(state): State<ApiState>,
    axum::Extension(credential): axum::Extension<super::auth::Credential>,
    ApiJson(request): ApiJson<dto::SetActRequest>,
) -> ApiResult<Json<dto::ActResultsDocument>> {
    set_act(
        state,
        credential,
        hekma_engine::RunLifecycleVerb::Pause,
        request,
    )
    .await
}

/// `POST /v0/runs/resume` — resume every Run of a selected set.
pub(crate) async fn runs_resume(
    State(state): State<ApiState>,
    axum::Extension(credential): axum::Extension<super::auth::Credential>,
    ApiJson(request): ApiJson<dto::SetActRequest>,
) -> ApiResult<Json<dto::ActResultsDocument>> {
    set_act(
        state,
        credential,
        hekma_engine::RunLifecycleVerb::Resume,
        request,
    )
    .await
}

/// `POST /v0/runs/drain` — drain every Run of a selected set, each with its
/// own bound (`{"set_token": …, "timeout_ms"?: n}`).
pub(crate) async fn runs_drain(
    State(state): State<ApiState>,
    axum::Extension(credential): axum::Extension<super::auth::Credential>,
    ApiJson(request): ApiJson<dto::SetActRequest>,
) -> ApiResult<Json<dto::ActResultsDocument>> {
    set_act(
        state,
        credential,
        hekma_engine::RunLifecycleVerb::Drain,
        request,
    )
    .await
}

/// `POST /v0/runs/close` — close every child Run of a selected set (a Run
/// that owns a process is that Run's `owns-process` refusal).
pub(crate) async fn runs_close(
    State(state): State<ApiState>,
    axum::Extension(credential): axum::Extension<super::auth::Credential>,
    ApiJson(request): ApiJson<dto::SetActRequest>,
) -> ApiResult<Json<dto::ActResultsDocument>> {
    if request.window_secs.is_some() || request.timeout_ms.is_some() {
        return Err(ApiError::invalid_body(
            "close takes only the set_token".to_string(),
        ));
    }
    let run_ids = consume_set(&state, credential, &request.set_token)?;
    let mut results = Vec::with_capacity(run_ids.len());
    for run_id in run_ids {
        results.push(match state.engine.close_run(&run_id).await {
            Ok(view) => dto::ActResult {
                run_id,
                outcome: "closed".to_string(),
                run: Some(view),
                ..dto::ActResult::default()
            },
            Err(error) => refused(run_id, error.into()),
        });
    }
    Ok(Json(dto::ActResultsDocument {
        schema_version: dto::ACT_RESULTS_SCHEMA_VERSION,
        dry_run: false,
        verb: "close",
        matched: results.len(),
        results,
    }))
}

/// `PATCH /v0/runs/labels` — bind and unbind labels on every Run of a
/// selected set as the Operator (`{"set_token": …, "set": {…},
/// "remove": […]}`), one result per Run.
pub(crate) async fn runs_labels_patch(
    State(state): State<ApiState>,
    axum::Extension(credential): axum::Extension<super::auth::Credential>,
    ApiJson(request): ApiJson<dto::SetLabelsRequest>,
) -> ApiResult<Json<dto::ActResultsDocument>> {
    let change = LabelChange {
        set: request.set,
        remove: request.remove,
    };
    change.validate()?;
    let run_ids = consume_set(&state, credential, &request.set_token)?;
    let mut results = Vec::with_capacity(run_ids.len());
    for run_id in run_ids {
        results.push(
            match state.engine.label_run(&run_id, change.clone(), false).await {
                Ok(outcome) => dto::ActResult {
                    run_id,
                    outcome: if outcome.changed {
                        "labeled"
                    } else {
                        "unchanged"
                    }
                    .to_string(),
                    labels: Some(outcome.labels),
                    prior_labels: Some(outcome.prior_labels),
                    ..dto::ActResult::default()
                },
                Err(error) => refused(run_id, error.into()),
            },
        );
    }
    Ok(Json(dto::ActResultsDocument {
        schema_version: dto::ACT_RESULTS_SCHEMA_VERSION,
        dry_run: false,
        verb: "label",
        matched: results.len(),
        results,
    }))
}

/// `POST /v0/runs/limits` — set or clear limits on every Run of a selected
/// set as the Operator, one result per Run (the outcome names the
/// direction: `raised`, `lowered`, `cleared`, `set`, `unchanged`).
pub(crate) async fn runs_limits(
    State(state): State<ApiState>,
    axum::Extension(credential): axum::Extension<super::auth::Credential>,
    ApiJson(request): ApiJson<dto::SetLimitsRequest>,
) -> ApiResult<Json<dto::ActResultsDocument>> {
    let (token, patch) = request.into_parts();
    let change = limit_change(&patch)?;
    let run_ids = consume_set(&state, credential, &token)?;
    let answered = state
        .engine
        .set_run_limits(run_ids, change, hekma_engine::LimitActor::Operator, false)
        .await?;
    let results: Vec<dto::ActResult> = answered
        .into_iter()
        .map(|(run_id, outcome)| match outcome {
            Ok(outcome) => dto::ActResult {
                run_id: run_id.to_string(),
                outcome: outcome.direction.as_str().to_string(),
                limits: Some(outcome),
                ..dto::ActResult::default()
            },
            Err(error) => refused(run_id.to_string(), error.into()),
        })
        .collect();
    Ok(Json(dto::ActResultsDocument {
        schema_version: dto::ACT_RESULTS_SCHEMA_VERSION,
        dry_run: false,
        verb: "budget",
        matched: results.len(),
        results,
    }))
}

// ---- Story 23-1: the endpoint bindings and the metering proxy ----

/// `GET /v0/bindings?after=&limit=` (Operator bearer only) — the keyset
/// listing by name.
pub(crate) async fn bindings_list(
    State(state): State<ApiState>,
    RawQuery(query): RawQuery,
) -> ApiResult<Json<dto::BindingsDocument>> {
    let (after, limit) = binding_page(query.as_deref());
    let bindings = state.engine.bindings(after.as_deref(), limit).await?;
    let next_after = bindings.last().map(|binding| binding.name.clone());
    Ok(Json(dto::BindingsDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        bindings,
        next_after,
    }))
}

/// Parse the listing query: `after` (the keyset cursor, a binding name) and
/// `limit` (1..=256, default 100). An unreadable query is the caller's
/// `400 invalid_body`.
fn binding_page(query: Option<&str>) -> (Option<String>, usize) {
    let mut after = None;
    let mut limit = 100usize;
    for pair in query.unwrap_or_default().split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key {
            "after" => after = Some(value.to_string()),
            "limit" => {
                if let Ok(parsed) = value.parse::<usize>() {
                    limit = parsed.clamp(1, hekma_engine::MAX_BINDINGS);
                }
            }
            _ => {}
        }
    }
    (after, limit)
}

/// `POST /v0/bindings` (Operator bearer only) — create one binding: the
/// prefix its calls arrive on, the upstream they forward to, the labels its
/// Runs carry, an optional custodied-key reference (inert until key
/// custody ships). `201` + `Location` + the binding; the change is one
/// event, committed before this answers.
pub(crate) async fn binding_create(
    State(state): State<ApiState>,
    ApiJson(spec): ApiJson<hekma_engine::BindingSpec>,
) -> ApiResult<(
    StatusCode,
    [(HeaderName, String); 1],
    Json<dto::BindingDocument>,
)> {
    let binding = state.engine.create_binding(spec).await?;
    let location = format!("/v0/bindings/{}", binding.name);
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(dto::BindingDocument {
            schema_version: dto::CONTROL_API_SCHEMA_VERSION,
            binding,
        }),
    ))
}

/// `GET /v0/bindings/{name}` (Operator bearer only) — one binding.
pub(crate) async fn binding_show(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<dto::BindingDocument>> {
    let binding = state
        .engine
        .binding(&name)
        .await?
        .ok_or_else(|| hekma_engine::BindingError::NotFound { name: name.clone() })?;
    Ok(Json(dto::BindingDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        binding,
    }))
}

/// `PATCH /v0/bindings/{name}` (Operator bearer only) — change the prefix,
/// the upstream, the whole label set or the custodied-key reference; a
/// label change re-resolves the binding's open Runs in the same job. One
/// event.
pub(crate) async fn binding_patch(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    ApiJson(patch): ApiJson<hekma_engine::BindingPatch>,
) -> ApiResult<Json<dto::BindingDocument>> {
    let binding = state.engine.update_binding(&name, patch).await?;
    Ok(Json(dto::BindingDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        binding,
    }))
}

/// `DELETE /v0/bindings/{name}` (Operator bearer only) — delete the
/// binding: its Run and its split Runs end with cause `binding-removed`,
/// and further calls on its prefix answer no route. One event.
pub(crate) async fn binding_delete(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    state.engine.delete_binding(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /v0/bindings/{name}/routes/{route}` (Operator bearer only) — add or
/// replace one reroute route (stored and validated now; reroute uses it
/// when it ships). One event, answered with the binding.
pub(crate) async fn binding_route_put(
    State(state): State<ApiState>,
    Path((name, route)): Path<(String, String)>,
    ApiJson(mut spec): ApiJson<hekma_engine::RouteSpec>,
) -> ApiResult<Json<dto::BindingDocument>> {
    spec.name = route;
    let binding = state.engine.put_binding_route(&name, spec).await?;
    Ok(Json(dto::BindingDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        binding,
    }))
}

/// `DELETE /v0/bindings/{name}/routes/{route}` (Operator bearer only) —
/// remove one route. One event, answered with the binding.
pub(crate) async fn binding_route_delete(
    State(state): State<ApiState>,
    Path((name, route)): Path<(String, String)>,
) -> ApiResult<Json<dto::BindingDocument>> {
    let binding = state.engine.remove_binding_route(&name, &route).await?;
    Ok(Json(dto::BindingDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        binding,
    }))
}

/// `POST /v0/bindings/{name}/credentials` (Operator bearer only, body
/// `{"class":"announce"|"proxy-access"}`) — issue the binding's credential
/// of that class, rotating any previous one in the same job. The token is
/// in this answer ONCE (its hash and token id are all that is stored). The
/// note says what accepts it: an announce credential announces beneath its
/// binding (story 23-2); a proxy-access one authenticates calls to the
/// metering proxy beyond loopback (story 22-5).
pub(crate) async fn binding_credential_issue(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<dto::BindingCredentialDocument>> {
    let class = credential_class(&body)?;
    let issued = state.engine.issue_binding_credential(&name, class).await?;
    Ok(Json(dto::BindingCredentialDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        class,
        token_id: issued.token_id.clone(),
        token: issued.token.expose_secret().to_string(),
        note: format!(
            "{} (token id {})",
            credential_note(class, &name),
            issued.token_id
        ),
    }))
}

/// What a freshly issued binding credential is for (story 23-2).
pub(crate) fn credential_note(
    class: hekma_engine::BindingCredentialClass,
    binding: &str,
) -> String {
    match class {
        hekma_engine::BindingCredentialClass::Announce => format!(
            "This credential is shown once and is not stored. It announces Runs beneath binding \
             `{binding}` on POST /v0/runs/announce, and heartbeats or closes the Runs it creates."
        ),
        hekma_engine::BindingCredentialClass::ProxyAccess => format!(
            "This credential is shown once and is not stored. Clients on other machines send it \
             as their API key to the metering proxy bound beyond loopback, on binding \
             `{binding}`'s prefix only."
        ),
    }
}

/// `POST /v0/bindings/{name}/credentials/revoke` (Operator bearer only,
/// body `{"class":…}`) — revoke the binding's active credential of that
/// class. One event.
pub(crate) async fn binding_credential_revoke(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<StatusCode> {
    let class = credential_class(&body)?;
    state.engine.revoke_binding_credential(&name, class).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Parse a credential route's body, `{"class":"announce"|"proxy-access"}`,
/// into its class. A blank body names no class, so it is refused the same
/// way as an unknown one.
fn credential_class(body: &[u8]) -> ApiResult<hekma_engine::BindingCredentialClass> {
    let request: dto::BindingCredentialRequest = if body.iter().all(u8::is_ascii_whitespace) {
        dto::BindingCredentialRequest::default()
    } else {
        serde_json::from_slice(body)
            .map_err(|err| ApiError::invalid_body(format!("invalid credential body: {err}")))?
    };
    hekma_engine::BindingCredentialClass::from_wire(&request.class).ok_or_else(|| {
        ApiError::invalid_body(format!(
            "unknown credential class `{}` (announce or proxy-access)",
            request.class
        ))
    })
}

/// `GET /v0/bindings/{name}/credentials` (Operator bearer only) — the
/// metadata listing: token ids, classes and states, never a value.
pub(crate) async fn binding_credentials(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> ApiResult<Json<dto::BindingCredentialsDocument>> {
    let credentials = state.engine.binding_credentials(&name).await?;
    Ok(Json(dto::BindingCredentialsDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        credentials,
    }))
}

/// `GET /v0/proxy` (Operator bearer only) — where the shared Metering
/// Story 24-2: the refusal where the OS has no hand-over.
fn upgrade_available(supported: bool) -> ApiResult<()> {
    if supported {
        return Ok(());
    }
    Err(ApiError::conflict_with_code(
        "upgrade-unsupported",
        "an in-place upgrade is not available on this operating system yet; restart the host \
         instead",
    ))
}

/// `POST /v0/host/upgrade` (story 24-2, Operator bearer only): hand this
/// Service Host over in place to a fresh copy of its binary — the one now
/// on disk, after an upgrade. Answers `202` once the request is taken;
/// the host then launches its successor and hands over to it, and the
/// successor's `control.json` names the new host. `409
/// upgrade-unsupported` where the OS has no handoff transport.
pub(crate) async fn host_upgrade(
    State(state): State<ApiState>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    upgrade_available(hekma_engine::handoff::supported())?;
    state.upgrade.notify_one();
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "upgrading": true })),
    ))
}

/// Proxy listens (`url`, `port`), or why it is not running (`error`).
pub(crate) async fn proxy_show(
    State(state): State<ApiState>,
) -> ApiResult<Json<dto::ProxyDocument>> {
    let settings = state.engine.proxy_settings().await?;
    Ok(Json(proxy_document(&state, settings)))
}

/// `PATCH /v0/proxy` (story 21-8, Operator bearer only) — turn the
/// outbound identity headers on or off: stored, published before this
/// answers, so the very next forwarded call follows it. Answers the
/// effective settings.
pub(crate) async fn proxy_update(
    State(state): State<ApiState>,
    ApiJson(request): ApiJson<dto::ProxySettingsRequest>,
) -> ApiResult<Json<dto::ProxyDocument>> {
    let settings = state
        .engine
        .set_proxy_identity_headers(request.identity_headers)
        .await?;
    Ok(Json(proxy_document(&state, settings)))
}

/// `GET /v0/retention` (story 24-4, Operator bearer only) — the two
/// retention windows and what compaction has done.
pub(crate) async fn retention_show(
    State(state): State<ApiState>,
) -> ApiResult<Json<dto::RetentionDocument>> {
    let view = state
        .engine
        .retention()
        .await
        .map_err(hekma_engine::RetentionError::Store)?;
    Ok(Json(dto::RetentionDocument::of(view)))
}

/// `PUT /v0/retention` (story 24-4, Operator bearer only) — change one or
/// both windows (1 to 3650 days; otherwise `400 invalid-retention`). The
/// Run window moves the listing's ended-Run cutoff and the compaction pass
/// together.
pub(crate) async fn retention_update(
    State(state): State<ApiState>,
    ApiJson(request): ApiJson<dto::RetentionRequest>,
) -> ApiResult<Json<dto::RetentionDocument>> {
    let view = state
        .engine
        .set_retention(hekma_engine::RetentionChange {
            run_retention_days: request.run_retention_days,
            calls_retention_days: request.calls_retention_days,
        })
        .await?;
    Ok(Json(dto::RetentionDocument::of(view)))
}

fn proxy_document(state: &ApiState, settings: hekma_engine::ProxySettings) -> dto::ProxyDocument {
    let info = state.engine.metering_proxy();
    dto::ProxyDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        running: info.is_some(),
        url: info.as_ref().map(|info| info.url.clone()),
        port: info.as_ref().map(|info| info.port),
        bind: info.as_ref().map(|info| info.bind.to_string()),
        tls: info.as_ref().is_some_and(|info| info.tls),
        public_url: info.as_ref().and_then(|info| info.public_url.clone()),
        error: state.engine.metering_proxy_error(),
        identity_headers: settings.identity_headers,
        stored_identity_headers: settings.stored_identity_headers,
        forced_by_flag: settings.forced_by_flag,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_committed_verb_whose_read_back_fails_is_still_an_act() {
        let document = acted(
            "run_A",
            Err(ApiError::new(
                super::super::error::ApiErrorKind::Internal,
                "the Fleet read failed".to_string(),
            )),
            None,
        );
        assert_eq!(document.run_id, "run_A");
        assert!(document.instance.is_none());
        let wire = serde_json::to_value(&document).unwrap();
        assert!(wire.get("instance").is_none(), "{wire}");
        assert!(wire.get("error").is_none(), "{wire}");
    }

    #[tokio::test]
    async fn an_upgrade_request_is_accepted_and_reaches_the_serve_loop() {
        let (state, _shutdown) = super::super::test_support::quiet_api_state();
        let refused = upgrade_available(false).unwrap_err();
        assert!(refused
            .error_object()
            .to_string()
            .contains("upgrade-unsupported"));
        if !hekma_engine::handoff::supported() {
            return;
        }
        let upgrade = std::sync::Arc::clone(&state.upgrade);
        let waiting = tokio::spawn(async move { upgrade.notified().await });
        tokio::task::yield_now().await;
        let (status, Json(body)) = host_upgrade(State(state)).await.unwrap();
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["upgrading"], true);
        tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("the serve loop was told")
            .unwrap();
    }
}
