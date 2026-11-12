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
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
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

/// `POST /v0/runs` — open a child Run (story 21-2). `201` + `Location`
/// with the new Run; a retried request carrying the same `external_id`
/// answers `201` with the SAME Run (the open is idempotent by external id).
pub(crate) async fn run_open(
    State(state): State<ApiState>,
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
    let view = state
        .engine
        .open_child_run(ChildRunRequest {
            parent,
            labels: request.labels.into_iter().collect(),
            limits: RunLimits {
                token_per_run: request.token_budget,
                dollar_per_run_micros,
                breach_action,
                ..RunLimits::default()
            },
            external_id: request.external_id,
        })
        .await?;
    let location = format!("/v0/runs/{}", view.record.run_id);
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(dto::RunDocument {
            schema_version: dto::CONTROL_API_SCHEMA_VERSION,
            run: view,
        }),
    )
        .into_response())
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

/// `POST /v0/runs/{id}/close` — close a child Run (idempotent; a Run that
/// owns a process is a `409` naming the stop remediation).
pub(crate) async fn run_close(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
) -> ApiResult<Json<dto::RunDocument>> {
    let view = state.engine.close_run(&run_id).await?;
    Ok(Json(dto::RunDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        run: view,
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
    let runs = state.engine.select_runs(&selector).await?;
    let minted = state
        .sets
        .mint(
            credential.0,
            runs.iter().map(|run| run.run_id.to_string()).collect(),
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
    let (instance, drain) = match verb {
        Verb::Stop => (state.engine.stop_run(run_id, window).await?, None),
        Verb::Pause => (state.engine.pause_run(run_id).await?, None),
        Verb::Resume => (state.engine.resume_run(run_id).await?, None),
        Verb::Drain => {
            let report = state.engine.drain_run(run_id, bound).await?;
            (report.instance, Some(report.outcome))
        }
    };
    let entry = state.engine.fleet_entry(instance.name.as_str()).await?;
    Ok(dto::RunActDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        run_id: run_id.to_string(),
        instance: entry,
        completed: drain.as_ref().map(|outcome| outcome.completed),
        waited_ms: drain.as_ref().map(|outcome| outcome.waited_ms),
    })
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
    Path(run_id): Path<String>,
    ApiJson(request): ApiJson<dto::RunLimitsPatch>,
) -> ApiResult<Json<dto::RunLimitsChangeDocument>> {
    let change = limit_change(&request)?;
    let mut results = state
        .engine
        .set_run_limits(
            vec![run_id],
            change,
            hekma_engine::LimitActor::Operator,
            request.dry_run,
        )
        .await?;
    let (_, outcome) = results.pop().ok_or_else(|| {
        ApiError::new(
            super::error::ApiErrorKind::Internal,
            "the budget job answered no Run",
        )
    })?;
    Ok(Json(dto::RunLimitsChangeDocument {
        schema_version: dto::CONTROL_API_SCHEMA_VERSION,
        outcome: outcome?,
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
    let mut tasks = Vec::with_capacity(run_ids.len());
    for run_id in run_ids.iter().cloned() {
        let state = state.clone();
        tasks.push(tokio::spawn(async move {
            let acted = act_one(&state, verb, &run_id, window, bound).await;
            (run_id, acted)
        }));
    }
    let mut results = Vec::with_capacity(tasks.len());
    for (task, run_id) in tasks.into_iter().zip(run_ids.iter()) {
        results.push(match task.await {
            Ok((run_id, Ok(document))) => dto::ActResult {
                run_id,
                outcome: past_tense(verb).to_string(),
                instance: Some(document.instance),
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
        verb: "limits",
        matched: results.len(),
        results,
    }))
}
