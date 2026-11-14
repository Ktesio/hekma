//! The Control API (story 17-3): an axum 0.8 HTTP/JSON surface over
//! `hekma-engine`'s public async facade, served from inside
//! `hekma serve --listen ...` — the AD-2 amendment's "its own Control-API
//! types" module.
//!
//! Layout: [`dto`] (the wire shapes), [`error`] (the engine-error → typed
//! status mapper), [`auth`] (the credential resolver layer), [`sse`] (the
//! live-events stream), [`routes`] (the handlers), [`tls`] (the `--listen`
//! TLS listener), and THIS file's [`ApiState`] + router + [`ControlApi`]
//! (the bind/serve/shutdown lifecycle the host drives).
//!
//! Lifecycle contract: story 21-4 makes the API exist on EVERY serve — a
//! loopback LOCAL endpoint (127.0.0.1, OS-chosen port, plain HTTP) the local
//! CLI finds through `control.json`, plus the operator's `--listen` listener
//! when configured (loopback plain, or TLS anywhere). Both listeners share
//! ONE router, ONE [`ApiState`] and ONE credential resolver. The API binds
//! AFTER the engine opens and the started notice lands; on shutdown it STOPS
//! ACCEPTING FIRST (graceful) on every listener — the open SSE streams END at
//! the same signal (a client sees the connection close; the engine is
//! unaffected), so graceful usually completes inside the bounded grace and
//! the abort backstop never fires; only then does the host's drain run.

pub(crate) mod auth;
mod dto;
pub(crate) mod error;
pub(crate) mod notes;
mod routes;
pub(crate) mod sets;
mod sse;
pub(crate) mod tls;

pub(crate) use auth::{CredentialClass, CredentialResolver};
pub(crate) use notes::NoteTap;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::middleware;
use axum::routing::{get, patch, post, put};
use axum::Router;
use hekma_engine::DiagnosticSink;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::Notifier;
use error::{ApiError, ApiErrorKind};

/// The per-server shared state: the engine the handlers project, the
/// credential resolver the auth layer checks, the config-write lock, the
/// API-level shutdown signal the SSE streams watch, and the activity clock
/// the auto-started host's idle exit reads (story 21-4).
#[derive(Clone)]
pub(crate) struct ApiState {
    /// The engine — the ONLY internal edge (AD-2): handlers call its PUBLIC
    /// async methods directly (they run ON the host's runtime; the blocking
    /// facade's `block_on` would panic there).
    pub(crate) engine: Arc<hekma_engine::Engine>,
    /// The host's credential resolver (hashes + classes, never a value).
    resolver: CredentialResolver,
    /// Serializes CONFIG WRITES per host (review item 8): a `PUT .../config`
    /// holds it across the engine write AND its read-back, so two concurrent
    /// PUTs serialize and each response's read-back reflects the value ITS
    /// request wrote (never a racing sibling's). Reads stay lock-free — a
    /// `GET` remains an unsynchronized snapshot of the effective config.
    config_write: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// The shutdown watch every SSE stream races its `recv` against: at
    /// shutdown the streams END (the cooperative close) instead of holding
    /// their connections open into the runtime's own teardown.
    shutdown: watch::Receiver<bool>,
    /// The request activity clock (story 21-4).
    activity: Activity,
    /// The tap over the engine's diagnostic sink (story 21-4): a write
    /// request's engine notes ride back to the client.
    notes: NoteTap,
    /// The engine's log-capture posture, read ONCE at bind (it is open-time
    /// only): `/v0/version` — the CLI's discovery probe — must never wait
    /// on an engine lock that a long `stop` holds (story 21-4 review).
    log_capture: hekma_engine::LogCaptureMode,
    /// The pending Run sets `POST /v0/runs/select` minted (story 21-6):
    /// host memory only, shared by every listener.
    sets: sets::SetStore,
    /// Story 22-1: whether THIS listener is loopback — a supervised Run's
    /// Run Key is refused beyond loopback (it has no network use). Each
    /// listener gets its own router over a state that says which it is.
    loopback: bool,
    /// Story 22-1: the rate-limited count of unresolved presentations,
    /// shared by every listener.
    pub(crate) auth_diagnostics: auth::AuthDiagnostics,
    /// Story 24-2: an Operator's in-place upgrade request reaches the serve
    /// loop here.
    pub(crate) upgrade: Arc<tokio::sync::Notify>,
    /// Story 24-2: what `GET /v0/host` answers (this host's mode and the
    /// service-manager check, settled at start).
    pub(crate) host: Arc<serde_json::Value>,
}

/// The API's request activity (story 21-4): how many authenticated requests
/// are in flight and when the last one began or ended — the auto-started
/// host idles out only with none in flight and none for its idle window.
#[derive(Clone)]
pub(crate) struct Activity {
    inner: Arc<ActivityInner>,
}

struct ActivityInner {
    origin: Instant,
    in_flight: AtomicUsize,
    last_ms: AtomicU64,
}

/// One in-flight request; dropping it ends the request (and stamps it).
pub(crate) struct ActivityGuard {
    inner: Arc<ActivityInner>,
}

impl Activity {
    /// A fresh clock: "the last activity" starts now (a host that has just
    /// started is not idle yet).
    pub(crate) fn new() -> Self {
        Activity {
            inner: Arc::new(ActivityInner {
                origin: Instant::now(),
                in_flight: AtomicUsize::new(0),
                last_ms: AtomicU64::new(0),
            }),
        }
    }

    fn stamp(inner: &ActivityInner) {
        let now = inner.origin.elapsed().as_millis() as u64;
        inner.last_ms.fetch_max(now, Ordering::AcqRel);
    }

    /// Begin one request.
    pub(crate) fn enter(&self) -> ActivityGuard {
        self.inner.in_flight.fetch_add(1, Ordering::AcqRel);
        Self::stamp(&self.inner);
        ActivityGuard {
            inner: Arc::clone(&self.inner),
        }
    }

    /// How long the API has been idle: `None` while a request is in flight,
    /// else the time since the last request began or ended.
    pub(crate) fn idle_for(&self) -> Option<Duration> {
        if self.inner.in_flight.load(Ordering::Acquire) > 0 {
            return None;
        }
        let now = self.inner.origin.elapsed().as_millis() as u64;
        let last = self.inner.last_ms.load(Ordering::Acquire);
        Some(Duration::from_millis(now.saturating_sub(last)))
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        Activity::stamp(&self.inner);
        self.inner.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The envelope for an UNMATCHED PATH (review item 9): the promise "every
/// error is a versioned JSON body — never a bare status" covers routing too,
/// not just handlers. Lives INSIDE the auth layer (the layer is applied over
/// the fallback), so an unmatched path still answers 401 without a bearer.
async fn fallback_not_found(uri: axum::http::Uri) -> ApiError {
    ApiError::new(
        ApiErrorKind::NotFound,
        format!("no such route: {}", uri.path()),
    )
}

/// The envelope for a KNOWN path with an UNSUPPORTED method (review item 9):
/// axum's default bare `405` would break the same promise. Also behind the
/// auth layer, for the same reason.
async fn method_not_allowed(method: axum::http::Method, uri: axum::http::Uri) -> ApiError {
    ApiError::new(
        ApiErrorKind::MethodNotAllowed,
        format!("{method} is not supported on {}", uri.path()),
    )
}

/// The v0 route table — every route behind the ONE auth layer (there are
/// no anonymous routes, `/v0/version` included: the acceptance criteria's
/// "every route answers 401 without a bearer" is structural, not per-handler
/// discipline).
fn router(state: ApiState) -> Router {
    Router::new()
        // Self-description.
        .route("/v0/version", get(routes::version))
        // Fleet reads.
        .route("/v0/fleet", get(routes::fleet))
        // Registration + per-instance surface.
        .route("/v0/instances", post(routes::register))
        .route(
            "/v0/instances/{name}",
            get(routes::show).delete(routes::remove),
        )
        .route("/v0/instances/{name}/status", get(routes::status))
        .route(
            "/v0/instances/{name}/capabilities",
            get(routes::capabilities),
        )
        .route(
            "/v0/instances/{name}/config",
            get(routes::config_get).put(routes::config_put),
        )
        // Story 22-2: a custodied key's reveal is recorded before it prints.
        .route(
            "/v0/instances/{name}/config/revealed",
            post(routes::config_revealed),
        )
        .route("/v0/instances/{name}/events", get(routes::events_pull))
        .route("/v0/instances/{name}/start", post(routes::start))
        .route("/v0/instances/{name}/stop", post(routes::stop))
        .route("/v0/instances/{name}/drain", post(routes::drain))
        .route("/v0/instances/{name}/pause", post(routes::pause))
        .route("/v0/instances/{name}/resume", post(routes::resume))
        // Story 21-4: the routes every CLI write needs (AD-20's one
        // data-path owner).
        .route("/v0/instances/{name}/send", post(routes::send))
        .route(
            "/v0/instances/{name}/memory",
            put(routes::memory_put).delete(routes::memory_delete),
        )
        .route(
            "/v0/instances/{name}/restart-policy",
            put(routes::restart_policy_put),
        )
        // Runs (story 21-2): open a child Run, read a Run, close a child.
        // Story 21-5: the listing (`GET /v0/runs`) — the same document
        // `hekma ps --json` prints.
        .route("/v0/runs", get(routes::runs_list).post(routes::run_open))
        .route("/v0/runs/{id}", get(routes::run_show))
        .route("/v0/runs/{id}/close", post(routes::run_close))
        // The per-call read surface over the calls store: the listing and
        // the by-id record, both in the Operator route family (the auth
        // layer's matrix — every other credential class answers 403).
        .route("/v0/calls", get(routes::calls_list))
        .route("/v0/calls/{call_id}", get(routes::call_show))
        // Story 22-1: Run Keys — trade a refresh credential (no id in the
        // path: the credential names its Run), revoke a Run's keys, list
        // a Run's key metadata.
        .route("/v0/runs/refresh", post(routes::run_refresh))
        .route("/v0/runs/announce", post(routes::run_announce))
        // Story 23-3: inbound OTLP/HTTP+JSON under an announce credential.
        .route("/v1/metrics", post(crate::otlp::intake::metrics_intake))
        .route("/v0/runs/{id}/heartbeat", post(routes::run_heartbeat))
        // Story 19-5: a Run's confirmed usage (Operator bearer only).
        .route(
            "/v0/runs/{id}/reconciliation",
            get(routes::run_reconciliations).post(routes::run_reconcile),
        )
        // Story 23-4: the hold family, single-Run and set forms.
        .route("/v0/runs/{id}/hold", post(routes::run_hold))
        .route("/v0/runs/{id}/release", post(routes::run_release))
        .route("/v0/runs/{id}/reject", post(routes::run_reject))
        .route("/v0/runs/{id}/reroute", post(routes::run_reroute))
        .route("/v0/runs/hold", post(routes::runs_hold))
        .route("/v0/runs/release", post(routes::runs_release))
        .route("/v0/runs/reject", post(routes::runs_reject))
        .route("/v0/runs/reroute", post(routes::runs_reroute))
        .route("/v0/runs/{id}/revoke-keys", post(routes::run_revoke_keys))
        .route("/v0/runs/{id}/keys", get(routes::run_keys))
        // Story 21-6: the Run-grain lifecycle verbs, Run-level limits, and
        // Selector sets (select once, act on exactly that set — the token
        // rides the body, never a URL).
        .route("/v0/runs/{id}/stop", post(routes::run_stop))
        .route("/v0/runs/{id}/pause", post(routes::run_pause))
        .route("/v0/runs/{id}/resume", post(routes::run_resume))
        .route("/v0/runs/{id}/drain", post(routes::run_drain))
        .route(
            "/v0/runs/{id}/limits",
            get(routes::run_limits_get).patch(routes::run_limits_patch),
        )
        .route("/v0/runs/select", post(routes::runs_select))
        .route("/v0/runs/stop", post(routes::runs_stop))
        .route("/v0/runs/pause", post(routes::runs_pause))
        .route("/v0/runs/resume", post(routes::runs_resume))
        .route("/v0/runs/drain", post(routes::runs_drain))
        .route("/v0/runs/close", post(routes::runs_close))
        .route("/v0/runs/labels", patch(routes::runs_labels_patch))
        .route("/v0/runs/limits", post(routes::runs_limits))
        // Bound labels (story 21-9): read a Run's labels, bind and unbind
        // them as the Operator (this bearer is the Operator's; the Run Key
        // and announce credential classes never reach these routes).
        .route(
            "/v0/runs/{id}/labels",
            get(routes::run_labels_get).patch(routes::run_labels_patch),
        )
        // Policies (story 21-3): Operator-set limits for the Runs a
        // Selector matches by bound label (this bearer is the Operator's).
        .route(
            "/v0/policies",
            get(routes::policies_list).post(routes::policy_create),
        )
        .route(
            "/v0/policies/{name}",
            get(routes::policy_show)
                .patch(routes::policy_change)
                .delete(routes::policy_delete),
        )
        // Endpoint bindings (story 23-1, Operator bearer only — the other
        // credential classes are refused on this family): the Operator
        // surface the shared Metering Proxy routes on, with per-binding
        // reroute routes and announce / proxy-access credentials.
        .route(
            "/v0/bindings",
            get(routes::bindings_list).post(routes::binding_create),
        )
        .route(
            "/v0/bindings/{name}",
            get(routes::binding_show)
                .patch(routes::binding_patch)
                .delete(routes::binding_delete),
        )
        .route(
            "/v0/bindings/{name}/routes/{route}",
            put(routes::binding_route_put).delete(routes::binding_route_delete),
        )
        .route(
            "/v0/bindings/{name}/credentials",
            get(routes::binding_credentials).post(routes::binding_credential_issue),
        )
        .route(
            "/v0/bindings/{name}/credentials/revoke",
            post(routes::binding_credential_revoke),
        )
        // Story 23-1: where the shared Metering Proxy listens (or why it is
        // not running).
        .route(
            "/v0/proxy",
            get(routes::proxy_show).patch(routes::proxy_update),
        )
        // Story 24-2: hand this host over to a fresh one in place.
        .route("/v0/host", get(routes::host_show))
        .route("/v0/host/upgrade", post(routes::host_upgrade))
        // Story 24-4: the retention windows and the compaction state.
        .route(
            "/v0/retention",
            get(routes::retention_show).put(routes::retention_update),
        )
        // Live events (SSE).
        .route("/v0/events", get(sse::live_events))
        // The ephemeral-host export surface (story 17-4): the global outbox
        // pull + the durable ack advance.
        .route("/v0/export", get(routes::export_pull))
        .route("/v0/export/ack", post(routes::export_ack))
        // Envelope the routing rejections TOO (review item 9): an unmatched
        // path is an envelope 404, a known path with the wrong method an
        // envelope 405 — never a bare status. Both run INSIDE the auth layer
        // (declared before `.layer`), so unauthenticated probes still get
        // the 401, not a route-map oracle.
        .fallback(fallback_not_found)
        .method_not_allowed_fallback(method_not_allowed)
        // Story 21-4: a write request's engine notes ride back in a header
        // (inside the auth layer: an unauthenticated request captures none).
        .layer(middleware::from_fn_with_state(
            state.clone(),
            notes::capture_engine_notes,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ))
        // Story 22-1: OUTSIDE the auth layer — a credential in a path or
        // query is refused (and revoked) before anything authenticates.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::refuse_credential_in_url,
        ))
        .with_state(state)
}

/// How long the graceful shutdown lets in-flight requests settle before the
/// abort backstop fires. The SSE streams end on the shutdown signal itself
/// (the cooperative close), so this is generous headroom for the rest — one
/// second is plenty for loopback reads.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// Test-only construction of an [`ApiState`] over a read-only engine on a
/// fresh temp state dir, with its shutdown SENDER so a test can fire the
/// cooperative close (the streams watch THIS channel; a separate
/// `watch::channel` in the test would never be observed).
///
/// The engine (and its runtime) are LEAKED, never dropped: these tests run
/// inside `#[tokio::test]`'s async context, where dropping a runtime
/// panics ("blocking is not allowed"), and the state's own temp root rides
/// `TempDir::into_path` for the same reason. Four seams, one process — the
/// leak is bounded and test-only.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn quiet_api_state() -> (ApiState, watch::Sender<bool>) {
        // Build on a plain thread: a blocking context for the engine's own
        // runtime construction, however the test is driven.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let root = tempfile::TempDir::new().expect("a temp state root");
            let engine = hekma_engine::Engine::open_read_only(Some(root.path().to_path_buf()))
                .expect("a read-only engine");
            let engine = std::sync::Arc::new(engine);
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let state = ApiState {
                engine: std::sync::Arc::clone(&engine),
                resolver: CredentialResolver::new([("quiet", CredentialClass::OperatorBearer)]),
                config_write: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                shutdown: shutdown_rx,
                activity: Activity::new(),
                notes: NoteTap::keeping_startup_notes(),
                log_capture: hekma_engine::LogCaptureMode::Full,
                sets: sets::SetStore::new(None),
                loopback: true,
                auth_diagnostics: auth::AuthDiagnostics::new(None, auth::AuthDiagnostics::INTERVAL),
                upgrade: Arc::new(tokio::sync::Notify::new()),
                host: Arc::new(serde_json::json!({ "handoff": false })),
            };
            // Leak the engine (never dropped) and the temp root.
            std::mem::forget(engine);
            std::mem::forget(root);
            let _ = tx.send((state, shutdown_tx));
        });
        rx.recv().expect("the quiet state")
    }

    /// Like [`quiet_api_state`] over a WRITABLE engine (story 21-5's SSE
    /// seam tests commit events through it). Leaked the same way.
    pub(crate) fn writable_api_state() -> (ApiState, watch::Sender<bool>) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let root = tempfile::TempDir::new().expect("a temp state root");
            let engine = hekma_engine::Engine::open(Some(root.path().to_path_buf()))
                .expect("a writable engine");
            let engine = std::sync::Arc::new(engine);
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let state = ApiState {
                engine: std::sync::Arc::clone(&engine),
                resolver: CredentialResolver::new([("quiet", CredentialClass::OperatorBearer)]),
                config_write: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                shutdown: shutdown_rx,
                activity: Activity::new(),
                notes: NoteTap::keeping_startup_notes(),
                log_capture: hekma_engine::LogCaptureMode::Full,
                sets: sets::SetStore::new(None),
                loopback: true,
                auth_diagnostics: auth::AuthDiagnostics::new(None, auth::AuthDiagnostics::INTERVAL),
                upgrade: Arc::new(tokio::sync::Notify::new()),
                host: Arc::new(serde_json::json!({ "handoff": false })),
            };
            std::mem::forget(engine);
            std::mem::forget(root);
            let _ = tx.send((state, shutdown_tx));
        });
        rx.recv().expect("the writable state")
    }
}

/// One listener the API serves on (story 21-4).
pub(crate) enum ListenerSpec {
    /// Plain HTTP on this address (the local endpoint; a loopback `--listen`).
    Plain(SocketAddr),
    /// HTTPS on this address (`--listen` with `--tls-cert`/`--tls-key`).
    Tls(SocketAddr, Arc<rustls::ServerConfig>),
}

/// The running Control API: the shutdown signal and the serve tasks (the
/// bound addresses are [`BoundListeners::local_addrs`]).
pub(crate) struct ControlApi {
    /// The shutdown signal the host fires on its own shutdown path.
    shutdown_tx: watch::Sender<bool>,
    /// The axum serve tasks, one per listener.
    tasks: Vec<JoinHandle<()>>,
    /// The TLS accept tasks (they own their sockets; aborted at shutdown).
    accept_tasks: Vec<JoinHandle<()>>,
    /// The request activity clock.
    activity: Activity,
    /// Story 24-2: the in-place upgrade requests.
    upgrade: Arc<tokio::sync::Notify>,
}

/// The Control API's sockets, BOUND but not yet serving (story 21-4): the
/// host records the local endpoint in `control.json` between
/// [`BoundListeners::bind`] and [`BoundListeners::serve`], so no request is
/// ever answered — and no "listening" notice printed — before the discovery
/// file is on disk. A connection that arrives in between waits in the
/// accept queue.
pub(crate) struct BoundListeners {
    bound: Vec<(
        tokio::net::TcpListener,
        SocketAddr,
        Option<Arc<rustls::ServerConfig>>,
    )>,
}

impl BoundListeners {
    /// Bind EVERY socket (one per spec) on the host's runtime; a failure
    /// leaves nothing bound. The message NAMES the address (AI-18).
    pub(crate) async fn bind(specs: Vec<ListenerSpec>) -> Result<Self, String> {
        let mut bound = Vec::with_capacity(specs.len());
        for spec in specs {
            let (addr, tls) = match spec {
                ListenerSpec::Plain(addr) => (addr, None),
                ListenerSpec::Tls(addr, config) => (addr, Some(config)),
            };
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|err| format!("could not bind the control api on {addr}: {err}"))?;
            let local_addr = listener.local_addr().map_err(|err| {
                format!("could not read the control api's bound address ({addr}): {err}")
            })?;
            bound.push((listener, local_addr, tls));
        }
        Ok(BoundListeners { bound })
    }

    /// The bound addresses, in spec order.
    pub(crate) fn local_addrs(&self) -> Vec<SocketAddr> {
        self.bound.iter().map(|(_, addr, _)| *addr).collect()
    }

    /// Start serving every bound socket, all sharing one router and one
    /// resolver. MUST be polled on the host runtime (the serve tasks live
    /// on it). The diagnostic `sink` is the host's notice stream: a serve
    /// task writes through it if it ends BEFORE the shutdown signal — the
    /// API is down for the rest of the host's life and that must be
    /// VISIBLE, not silent (AI-18).
    pub(crate) async fn serve(
        self,
        engine: Arc<hekma_engine::Engine>,
        resolver: CredentialResolver,
        notes: NoteTap,
        sink: DiagnosticSink,
        set_token_ttl: Option<Duration>,
        host: serde_json::Value,
    ) -> Result<ControlApi, String> {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let activity = Activity::new();
        let log_capture = engine.log_capture_mode().await;
        let state = ApiState {
            engine,
            resolver,
            config_write: Arc::new(tokio::sync::Mutex::new(())),
            shutdown: shutdown_rx.clone(),
            activity: activity.clone(),
            notes,
            log_capture,
            sets: sets::SetStore::new(set_token_ttl),
            loopback: true,
            auth_diagnostics: auth::AuthDiagnostics::new(
                Some(Notifier::over(sink.clone())),
                auth::AuthDiagnostics::INTERVAL,
            ),
            upgrade: Arc::new(tokio::sync::Notify::new()),
            host: Arc::new(host),
        };
        let upgrade = Arc::clone(&state.upgrade);
        let mut tasks = Vec::with_capacity(self.bound.len());
        let mut accept_tasks = Vec::new();
        for (listener, local_addr, tls) in self.bound {
            // One router per listener, sharing every piece of state but the
            // loopback fact (story 22-1, P5).
            let app = router(ApiState {
                loopback: local_addr.ip().is_loopback(),
                ..state.clone()
            });
            let notifier = Notifier::over(sink.clone());
            let task = match tls {
                None => spawn_serve(listener, app.clone(), shutdown_rx.clone(), notifier),
                Some(config) => {
                    let (tls_listener, accept) =
                        tls::TlsListener::new(listener, config).map_err(|err| {
                            format!("could not start TLS on the control api ({local_addr}): {err}")
                        })?;
                    accept_tasks.push(accept);
                    spawn_serve(tls_listener, app.clone(), shutdown_rx.clone(), notifier)
                }
            };
            tasks.push(task);
        }
        Ok(ControlApi {
            shutdown_tx,
            tasks,
            accept_tasks,
            activity,
            upgrade,
        })
    }
}

impl ControlApi {
    /// The request activity clock (the auto-started host's idle exit).
    /// Story 24-2: the in-place upgrade requests (`POST /v0/host/upgrade`).
    pub(crate) fn upgrade_requests(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.upgrade)
    }

    pub(crate) fn activity(&self) -> Activity {
        self.activity.clone()
    }

    /// Stop accepting on every listener, and let in-flight requests settle
    /// within [`SHUTDOWN_GRACE`]. The signal ALSO ends every open SSE stream
    /// (the cooperative close), so graceful shutdown normally completes well
    /// inside the grace; the abort below is the bounded BACKSTOP for a client
    /// that never lets go. Returns `true` when that backstop FIRED so the host
    /// can surface it (AI-18). MUST be polled on the host runtime.
    /// Self-consuming: after this the API is down for good.
    pub(crate) async fn shutdown(self) -> bool {
        let _ = self.shutdown_tx.send(true);
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        let mut aborted = false;
        for mut task in self.tasks {
            if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
                task.abort();
                // Reap the aborted task HERE (bounded, on a polled task).
                let _ = task.await;
                aborted = true;
            }
        }
        for accept in self.accept_tasks {
            accept.abort();
            let _ = accept.await;
        }
        aborted
    }
}

/// Serve `app` on one listener until the shutdown signal, surfacing an early
/// end (the serve task died before the signal) as a loud notice.
fn spawn_serve<L>(
    listener: L,
    app: Router,
    shutdown_rx: watch::Receiver<bool>,
    notifier: Notifier,
) -> JoinHandle<()>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
{
    tokio::spawn(async move {
        // The graceful future gets its OWN receiver clone — the ORIGINAL
        // stays live in this task for the early-death check below.
        let mut signal_rx = shutdown_rx.clone();
        let shutdown = async move {
            // Fired when the host's shutdown path sends `true` (and on sender
            // drop — a host panic still closes the API).
            let _ = signal_rx.wait_for(|ready| *ready).await;
        };
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await;
        // The serve future ENDED. If the host's signal fired, this is the
        // normal graceful path; if NOT, the API died under us. Surface it
        // loudly (AI-18): the operator must know the API is down while
        // supervision continues — never a silent task end.
        if !*shutdown_rx.borrow() {
            notifier.note(
                "control api DOWN: its serve task ended before shutdown \
                 (accept error or internal failure) — no further requests \
                 will be answered; agent supervision continues",
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_activity_clock_counts_in_flight_requests_and_idle_time() {
        let activity = Activity::new();
        // Nothing in flight: idle (for ~0 ms — a fresh host is not idle yet).
        assert!(activity.idle_for().unwrap() < Duration::from_secs(5));
        let guard = activity.enter();
        assert_eq!(activity.idle_for(), None, "a request is in flight");
        let second = activity.enter();
        drop(guard);
        assert_eq!(activity.idle_for(), None, "one is still in flight");
        drop(second);
        assert!(activity.idle_for().is_some());
    }
}
