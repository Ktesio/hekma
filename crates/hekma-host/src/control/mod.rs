//! The Control API (story 17-3): an axum 0.8 HTTP/JSON surface over
//! `hekma-engine`'s public async facade, served from inside
//! `hekma serve --listen ...` — the AD-2 amendment's "its own Control-API
//! types" module.
//!
//! Layout: [`dto`] (the wire shapes), [`error`] (the engine-error → typed
//! status mapper), [`auth`] (the bearer layer), [`sse`] (the live-events
//! stream), [`routes`] (the handlers), and THIS file's [`ApiState`] +
//! router + [`ControlApi`] (the bind/serve/shutdown lifecycle the host
//! drives).
//!
//! Lifecycle contract (the frozen mount task): the API exists ONLY when
//! `--listen` is configured (default-off — no flag, no API, serve stays
//! byte-identical to 17-2); it binds AFTER the engine opens and the started
//! notice lands; on shutdown it STOPS ACCEPTING FIRST (graceful) — the open
//! SSE streams END at the same signal (a client sees the connection close;
//! the engine is unaffected), so graceful usually completes inside the
//! bounded grace and the abort backstop never fires; only then does the
//! host's drain run.

mod auth;
mod dto;
mod error;
mod routes;
mod sse;

pub(crate) use auth::BearerToken;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::middleware;
use axum::routing::get;
use axum::routing::post;
use axum::Router;
use hekma_engine::DiagnosticSink;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::Notifier;
use error::{ApiError, ApiErrorKind};

/// The per-server shared state: the engine the handlers project, the
/// startup-resolved bearer token the auth layer checks, the config-write
/// lock, and the API-level shutdown signal the SSE streams watch.
#[derive(Clone)]
pub(crate) struct ApiState {
    /// The engine — the ONLY internal edge (AD-2): handlers call its PUBLIC
    /// async methods directly (they run ON the host's runtime; the blocking
    /// facade's `block_on` would panic there).
    engine: Arc<hekma_engine::Engine>,
    /// The resolved bearer secret.
    bearer: BearerToken,
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

/// The v0 route table — every route behind the ONE bearer layer (there are
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
        .route("/v0/instances/{name}/events", get(routes::events_pull))
        .route("/v0/instances/{name}/start", post(routes::start))
        .route("/v0/instances/{name}/stop", post(routes::stop))
        .route("/v0/instances/{name}/pause", post(routes::pause))
        .route("/v0/instances/{name}/resume", post(routes::resume))
        // Live events (SSE).
        .route("/v0/events", get(sse::live_events))
        // Envelope the routing rejections TOO (review item 9): an unmatched
        // path is an envelope 404, a known path with the wrong method an
        // envelope 405 — never a bare status. Both run INSIDE the auth layer
        // (declared before `.layer`), so unauthenticated probes still get
        // the 401, not a route-map oracle.
        .fallback(fallback_not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ))
        .with_state(state)
}

/// How long the graceful shutdown lets in-flight requests settle before the
/// abort backstop fires. The SSE streams end on the shutdown signal itself
/// (the cooperative close), so this is generous headroom for the rest — one
/// second is plenty for loopback reads.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// The running Control API: the bound address (the OS-chosen port when the
/// operator passed `:0`), the shutdown signal, and the serve task.
pub(crate) struct ControlApi {
    /// The address the API answers on (resolved after the bind — `:0`
    /// becomes the real host:port the notice reports and tests parse).
    local_addr: SocketAddr,
    /// The shutdown signal the host fires on its own shutdown path.
    shutdown_tx: watch::Sender<bool>,
    /// The axum serve task (the abort backstop fires only if a client
    /// ignores the stream-end signal and holds the graceful stop past the
    /// grace).
    task: JoinHandle<()>,
}

impl ControlApi {
    /// Bind the API on the host's runtime. MUST be polled on that runtime
    /// (the host does `rt.block_on(ControlApi::bind(...))`); the serve task
    /// then lives on the same runtime. A bind failure (address in use, no
    /// permission) is the caller's surfaced `Startup` error — the message
    /// NAMES the address (AI-18). The diagnostic `sink` is the host's notice
    /// stream: the serve task writes through it if it ends BEFORE the
    /// shutdown signal (an accept error or an internal failure) — the API is
    /// down for the rest of the host's life and that must be VISIBLE, not
    /// silent (review item 3 / AI-18).
    pub(crate) async fn bind(
        addr: SocketAddr,
        engine: Arc<hekma_engine::Engine>,
        bearer: BearerToken,
        sink: DiagnosticSink,
    ) -> Result<Self, String> {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|err| format!("could not bind the control api on {addr}: {err}"))?;
        let local_addr = listener.local_addr().map_err(|err| {
            format!("could not read the control api's bound address ({addr}): {err}")
        })?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        // The streams get their OWN receiver clone (each races its recv
        // against the same signal); the serve task's graceful future uses
        // the original.
        let app = router(ApiState {
            engine,
            bearer,
            config_write: Arc::new(tokio::sync::Mutex::new(())),
            shutdown: shutdown_rx.clone(),
        });
        let task = tokio::spawn(async move {
            let shutdown_rx = shutdown_rx;
            let notifier = Notifier::over(sink);
            // The graceful future gets its OWN receiver clone — the ORIGINAL
            // stays live in this task for the early-death check below.
            let mut signal_rx = shutdown_rx.clone();
            let shutdown = async move {
                // Fired when the host's shutdown path sends `true` (and on
                // sender drop — a host panic still closes the API).
                let _ = signal_rx.wait_for(|ready| *ready).await;
            };
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await;
            // The serve future ENDED. If the host's signal fired, this is the
            // normal graceful path; if NOT, the API died under us (an accept
            // loop error, a panicking connection task taking the server down,
            // an internal failure). Surface it loudly (AI-18): the operator
            // must know the API is down while supervision continues — never
            // a silent task end.
            if !*shutdown_rx.borrow() {
                notifier.note(
                    "control api DOWN: its serve task ended before shutdown \
                     (accept error or internal failure) — no further requests \
                     will be answered; agent supervision continues",
                );
            }
        });
        Ok(ControlApi {
            local_addr,
            shutdown_tx,
            task,
        })
    }

    /// The address clients use (the notice's address of record).
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Whether the API is bound on a loopback address (`127.0.0.0/8`,
    /// `::1`) — the host warns at startup when it is NOT (review item 15:
    /// the API has no TLS and bearer-only auth; a non-loopback bind is
    /// legitimate — 17-5's container recipe — but it must warn).
    pub(crate) fn is_loopback(&self) -> bool {
        self.local_addr.ip().is_loopback()
    }

    /// Stop accepting, and let in-flight requests settle within
    /// [`SHUTDOWN_GRACE`]. The signal ALSO ends every open SSE stream (the
    /// cooperative close — the spec's outcome: the client sees the
    /// connection close, the engine is unaffected), so graceful shutdown
    /// normally completes well inside the grace; the abort below is the
    /// bounded BACKSTOP for a client that never lets go. Returns `true` when
    /// that backstop FIRED (a connection was still open past the grace and
    /// was aborted) so the host can surface it — a degraded shutdown is a
    /// notice, never a silent event (review item 16 / AI-18). MUST be polled
    /// on the host runtime. Self-consuming: after this the API is down for
    /// good (the host calls it once, before the drain).
    pub(crate) async fn shutdown(self) -> bool {
        let _ = self.shutdown_tx.send(true);
        // Bounded wait on the serve task: graceful stop is quick now that
        // the streams end on the signal — but a client holding a request
        // open would still hang it, so the timeout owns the JoinHandle and
        // the abort below is guaranteed (a dropped JoinHandle would only
        // DETACH, leaking the half-stopped task through the drain).
        let mut task = self.task;
        if tokio::time::timeout(SHUTDOWN_GRACE, &mut task)
            .await
            .is_err()
        {
            task.abort();
            // Reap the aborted task HERE (bounded, on a polled task) rather
            // than leaving its teardown to straggle into the runtime's own
            // shutdown.
            let _ = task.await;
            return true;
        }
        false
    }
}
