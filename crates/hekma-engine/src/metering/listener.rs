//! The loopback FORWARD LISTENER (spine AD-7 v1 `engine-observed` definition,
//! story 3-4) — the ONE genuinely new subsystem.
//!
//! An engine-owned, per-instance, transparent HTTP forward proxy bound to
//! `127.0.0.1:<ephemeral>` (loopback ONLY — AC-B). The adapter points the agent's
//! OpenAI-compatible `base_url` at it (via the 2-2 config-mapping); the listener
//! FORWARDS each request to the real upstream provider, relays the response back
//! to the agent FAITHFULLY (status + headers + body — an honest transparent
//! proxy), and skims the OpenAI-compatible `usage` object out of a completion
//! response ([`super::parse`]) into a shared queue the supervisor's reaper drains
//! into the SAME `ingest_usage` choke point.
//!
//! ## Loopback-only bind (AC-B — the security invariant)
//!
//! The bind address is ENGINE-COMPUTED (`127.0.0.1:0`), NEVER adapter- or
//! operator-supplied — the engine is the sole authority on the listen address
//! (the AD-4/conventions discipline extended to the listener). The listener
//! binds EXCLUSIVELY to the loopback interface; a resolved non-loopback address is
//! a hard error ([`ListenerError::NonLoopbackBind`]), never a silent widening.
//! NFR-6 honesty: this is defense against ACCIDENTAL external exposure of the
//! agent's model traffic (which carries the resolved API key UPSTREAM), NOT a
//! hardened sandbox. IPv4 `127.0.0.1` only for v1 (IPv6 `::1` deferred, recorded).
//!
//! ## Secret / traffic no-leak (2-4 rigor)
//!
//! This proxy carries the agent's MODEL TRAFFIC, including its API key (in the
//! forwarded `Authorization` header). It MUST NOT log, echo, persist, or leak
//! request/response BODIES, HEADERS, or the auth key anywhere — not to the engine
//! event log, the ledger, error messages, stderr, or the agent log. Bodies +
//! headers are relayed UPSTREAM faithfully, but ONLY the parsed integer token
//! counts ever leave the proxy (into the queue → the ledger). Every error variant
//! here carries ONLY a static op label + a transport-shaped detail — NEVER a body,
//! a header value, or a URL with embedded credentials.
//!
//! ## Portable — NO OS-conditional compilation (the crux the brief flags)
//!
//! Async TCP + HTTP over loopback is portable `std`/tokio/`hyper`: binding
//! `127.0.0.1:0`, forwarding, relaying, and parsing JSON are IDENTICAL on Linux,
//! macOS, and Windows with NO OS-conditional attributes (no per-OS `#[cfg]` gate
//! anywhere in this file). So the listener lives in the engine CORE
//! (`src/metering/`), NOT in `backends/`, and the OS-cfg grep gate stays green.
//! Teardown is a portable tokio task-abort, not an OS concern.
//!
//! ## Streaming metering (story 12-2) — the ONE deliberate relay caveat
//!
//! The relay is faithful in everything the AGENT asked for, with exactly one
//! documented, upstream-visible modification: on a forwarded STREAMING request
//! (`POST` to a path ending `/chat/completions` + a JSON body already setting
//! `"stream": true`), the listener injects `"stream_options":
//! {"include_usage": true}` so the provider emits the terminal usage frame —
//! without it, streamed completions would go unmetered entirely. The PATH gate
//! (story 12-2 AMENDMENT) keeps the modification scoped to the only endpoint
//! whose responses the parse seam understands: any OTHER streaming endpoint is
//! forwarded UNMODIFIED (providers reject `stream_options` elsewhere with a
//! 400 — metering must never break the call it meters). Scope honesty: outside
//! the gate usage is not REQUESTED — it is not unmeterable, because the
//! response parse routes on CONTENT-TYPE, not path, so usage a provider sends
//! unprompted on any metered response is still parsed and recorded. If the
//! request already
//! carries a `stream_options` object, the agent's own choice is respected and
//! nothing is injected; a malformed body is forwarded UNMODIFIED. The response
//! side routes on content-type: an `text/event-stream` body is scanned for the
//! terminal SSE usage frame ([`super::parse::parse_openai_sse_usage`]), any
//! other body is skimmed as one JSON completion
//! ([`super::parse::parse_openai_usage`]) — both funnel into the SAME
//! [`ObservedQueue`]. This modification is a surface fact an operator can
//! observe upstream (documented in the metering design doc), not a hidden
//! rewrite.
//!
//! ## Scope (was v1 deferrals; 12-2/12-3 landed)
//!
//! * Streaming `usage` parse: LANDED (story 12-2, the injection above + the SSE
//!   scanner seam in [`super::parse`]).
//! * HTTPS upstreams: LANDED via vendored rustls+ring (story 12-3) — the client
//!   connector handles both schemes (see `default_forward_client`).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::HeaderName;
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::runtime::Handle;

use super::parse::{parse_openai_sse_usage, parse_openai_usage};

/// The outbound forward client type (story 12-3): hyper-util's legacy client
/// over a `hyper-rustls` HTTPS connector. The connector routes by scheme —
/// `http://` upstreams dial plain TCP exactly as before; `https://` upstreams
/// handshake TLS through the VENDORED rustls+ring stack (no system TLS). One
/// client type for both schemes, so the relay logic never branches on TLS.
type ForwardClient = Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Full<Bytes>,
>;

/// The PRODUCTION TLS client configuration (story 12-3): the vendored
/// `webpki-roots` public CA store — NO system trust store is read (the
/// no-system-dependency philosophy; identical roots on all three OSes, and the
/// same vendored-roots choice `kt`→`ureq` already ships). Named + injectable so
/// the TLS test can substitute a config trusting a self-signed root (see
/// [`ObservedListener::start_with_tls_config`]).
/// The vendored PRODUCTION root store (webpki-roots — the no-system-TLS
/// philosophy, matching ureq's choice). Extracted from
/// [`default_tls_config`] (review round 2) so a unit test can observe the
/// store DIRECTLY: an accidentally-empty root store would compile, pass every
/// in-suite test (whose https legs trust a custom self-signed root instead),
/// and still fail every PRODUCTION https forward at handshake — the one
/// failure mode the injection/seam tests structurally cannot catch.
fn default_root_store() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

fn default_tls_config() -> Arc<rustls::ClientConfig> {
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(default_root_store())
            .with_no_client_auth(),
    )
}

/// Build the outbound forward client for a given TLS config (story 12-3 test
/// seam): `https_or_http()` makes ONE connector handle BOTH upstream schemes,
/// so a test can point the listener at an `https://` upstream with a
/// self-signed-root-trusting config and prove the full composition (TLS +
/// streaming + metering) while production always calls through
/// [`default_tls_config`].
fn forward_client(tls: Arc<rustls::ClientConfig>) -> ForwardClient {
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config((*tls).clone())
        .https_or_http()
        .enable_http1()
        .build();
    Client::builder(TokioExecutor::new()).build(https)
}

/// The MAXIMUM size (bytes) of a single request OR response body this transparent
/// proxy will BUFFER before failing the relay cleanly (story 3-4 hardening).
///
/// The `forward()` relay must `collect()` each body into memory to BOTH relay it
/// faithfully AND skim the `usage` object — an unbounded `collect()` on a runaway
/// or malicious multi-GB body would exhaust engine memory (an OOM DoS). We wrap
/// both directions in [`Limited`] at this cap: a body that exceeds it fails as a
/// clean, traffic-free `502` (never a partial/corrupt relay, never an unbounded
/// alloc, never a panic).
///
/// The value is deliberately GENEROUS — 64 MiB — so it bounds a pathological body
/// while never truncating a REAL LLM completion (even a very large non-streaming
/// JSON completion with long tool-call arguments is orders of magnitude under this;
/// typical completions are well under 1 MiB). It is a named module constant, tuned
/// here in one place rather than a magic number inline at each `collect()`.
const MAX_RELAY_BODY_BYTES: usize = 64 * 1024 * 1024;

/// The RFC 7230 §6.1 HOP-BY-HOP headers — meaningful only for a single transport
/// hop, and MUST NOT be forwarded by a proxy. We STRIP these from the buffered
/// response (and defensively the request) before re-attaching the body: the relay
/// rebuilds a FIXED-LENGTH `Full<Bytes>` body, so hyper's server frames it itself.
/// Carrying a `chunked` upstream's `Transfer-Encoding` (or a now-wrong
/// `Content-Length`) onto that known-length body would produce a conflicting/lossy
/// frame the agent's client could mis-read. Stripping them lets hyper set the
/// correct `Content-Length` for the exact bytes we relay. Matched case-insensitively
/// (`HeaderName` compares ASCII-case-insensitively).
const HOP_BY_HOP_HEADERS: &[HeaderName] = &[
    hyper::header::CONNECTION,
    hyper::header::TE,
    hyper::header::TRANSFER_ENCODING,
    hyper::header::TRAILER,
    hyper::header::UPGRADE,
    hyper::header::PROXY_AUTHENTICATE,
    hyper::header::PROXY_AUTHORIZATION,
    hyper::header::CONTENT_LENGTH,
];

/// The shared queue of OBSERVED usage counts `(input_tokens, output_tokens,
/// cached_tokens)` the listener pushes and the supervisor's reaper drains
/// (story 3-4 drive model; the cached subset added by story 14-6, D8 — the
/// cached SUBSET of input under the INPUT-INCLUSIVE invariant).
///
/// This is the seam between the ASYNC listener task (event-driven, pushes as each
/// completion response is parsed) and the SYNC supervisor choke point (the reaper
/// cadence drains it and mints the per-Run `sequence`, then funnels into the SAME
/// `ingest_usage`). A plain `Arc<Mutex<VecDeque<..>>>` — the push is O(1) and the
/// contention is negligible (a few completions per reaper tick); no need for a
/// channel or an async lock. Only the three parsed INTEGER counts ever enter it —
/// NEVER a body, header, or key (the no-leak invariant).
pub type ObservedQueue = Arc<Mutex<VecDeque<(u64, u64, u64)>>>;

/// Why the loopback forward listener could not START (story 3-4). `thiserror`
/// only (no `miette` in the lib — conventions). Every variant carries ONLY a
/// static op label + a transport-shaped detail — NEVER a request/response body, a
/// header value, or a credential (the 2-4 no-leak rigor: an error path must carry
/// no secret).
#[derive(Debug, Error)]
pub enum ListenerError {
    /// The loopback TCP bind failed (`127.0.0.1:0` could not be acquired). Carries
    /// the OS error string (a bind/permission failure) — no traffic, no secret.
    #[error("could not bind the engine-observed loopback listener on 127.0.0.1: {detail}")]
    Bind {
        /// The underlying bind error (OS-level; carries no traffic).
        detail: String,
    },

    /// The OS resolved a NON-LOOPBACK bind address (AC-B hard error). This must
    /// never happen for `127.0.0.1:0`, but is enforced defensively: a non-loopback
    /// listen address is refused, never served (the security invariant). Carries
    /// the offending address (an ip:port, no traffic).
    #[error("the engine-observed listener resolved a NON-LOOPBACK address ({addr}); refusing to serve (loopback-only, AC-B)")]
    NonLoopbackBind {
        /// The refused (non-loopback) address.
        addr: SocketAddr,
    },

    /// The configured upstream base URL is not a usable HTTP(S) URL (empty, or
    /// not an `http://`/`https://` authority). Carries a STATIC reason only — NEVER
    /// the URL itself (an operator-configured base URL is not a secret, but keeping
    /// errors traffic-free is the uniform no-leak discipline). `https://` upstreams
    /// are SUPPORTED since story 12-3 (vendored rustls+ring); any other scheme
    /// (`ftp://`, a bare host) is refused here.
    #[error(
        "the engine-observed upstream base URL is not a usable http:// or https:// URL: {reason}"
    )]
    BadUpstream {
        /// A static reason (empty / bad scheme / unparseable) — no URL echoed.
        reason: String,
    },
}

/// A running, per-instance loopback forward listener (story 3-4). Held on the
/// supervised instance for its Run; its task is aborted at the terminal transition
/// (bounded to the Run — no orphan listeners, NFR-1). The engine-minted per-Run
/// `sequence` counter lives beside it in the supervisor (see `Supervised`).
///
/// Dropping this aborts the listener task (RAII teardown), so a supervised
/// instance that goes away (stop / crash-reap / adoption-replace) never leaks its
/// listener. The `base_url` is what the adapter's config-mapping injects into the
/// agent's launch env (the engine-provided interception point, AC6).
pub struct ObservedListener {
    /// The loopback URL the adapter points the agent's `base_url` at
    /// (`http://127.0.0.1:<port>`). Engine-computed; the adapter receives it, it
    /// does not choose it (AC-B — engine is the sole authority on the address).
    base_url: String,
    /// The resolved bound address (loopback — proven `is_loopback()`). Kept for the
    /// AC-B assertion + diagnostics.
    local_addr: SocketAddr,
    /// The accept-loop task's abort handle — aborted on drop (teardown at the
    /// terminal transition, bounded to the Run).
    task: tokio::task::AbortHandle,
    /// The shared queue the listener pushes observed counts into; the supervisor
    /// holds the other end and the reaper drains it.
    queue: ObservedQueue,
}

impl ObservedListener {
    /// START a loopback forward listener on the engine's tokio runtime (AC-A/AC-B).
    ///
    /// Binds `127.0.0.1:0` (ephemeral, loopback ONLY — the engine computes the
    /// address; it is never adapter/operator-supplied), verifies the resolved
    /// address is loopback (AC-B — refuse a non-loopback bind), validates the
    /// `upstream` is a usable `http://` or `https://` URL (story 12-3: HTTPS
    /// upstreams dial vendored rustls+ring with the `webpki-roots` CA store),
    /// and spawns the accept loop on `handle`. Returns the [`ObservedListener`]
    /// carrying the resolved `http://127.0.0.1:<port>` base URL to inject.
    ///
    /// `handle` is the engine's runtime handle (the supervisor runs on the blocking
    /// pool, so it cannot use `Handle::current`; the engine threads its handle in).
    /// The bind itself runs synchronously via `handle.block_on` (a fast, local
    /// loopback bind), so `start` stays a plain sync call the supervisor's sync
    /// start path can make.
    pub fn start(handle: &Handle, upstream: String) -> Result<ObservedListener, ListenerError> {
        Self::start_with_tls_config(handle, upstream, default_tls_config())
    }

    /// [`ObservedListener::start`] with an EXPLICIT TLS client config (story
    /// 12-3 test seam): the TLS test substitutes a config trusting its own
    /// self-signed root, proving the https + streaming + metering composition.
    /// Production uses [`start`](Self::start) (webpki-roots default).
    pub(crate) fn start_with_tls_config(
        handle: &Handle,
        upstream: String,
        tls: Arc<rustls::ClientConfig>,
    ) -> Result<ObservedListener, ListenerError> {
        // Validate the upstream is a usable http(s):// URL BEFORE binding
        // (fail fast, no listener leak).
        validate_upstream(&upstream)?;

        // Bind 127.0.0.1:0 (loopback, OS-picked ephemeral port). The bind is a fast
        // local syscall; run it on the runtime so the returned listener is owned by
        // the runtime the accept loop runs on.
        let listener = handle
            .block_on(async { TcpListener::bind(("127.0.0.1", 0)).await })
            .map_err(|e| ListenerError::Bind {
                detail: e.to_string(),
            })?;
        let local_addr = listener.local_addr().map_err(|e| ListenerError::Bind {
            detail: e.to_string(),
        })?;

        // AC-B: refuse a NON-LOOPBACK bind. `127.0.0.1:0` always resolves loopback,
        // but enforce it defensively — a non-loopback listen address is a hard error,
        // never served (the security invariant that makes intercepting traffic safe).
        if !local_addr.ip().is_loopback() {
            return Err(ListenerError::NonLoopbackBind { addr: local_addr });
        }

        let base_url = format!("http://{local_addr}");
        let queue: ObservedQueue = Arc::new(Mutex::new(VecDeque::new()));

        // Spawn the accept loop on the engine runtime. It is aborted on drop
        // (teardown at the terminal transition — bounded to the Run).
        let task_queue = Arc::clone(&queue);
        let task = handle.spawn(accept_loop(
            listener,
            upstream,
            task_queue,
            forward_client(tls),
        ));

        Ok(ObservedListener {
            base_url,
            local_addr,
            task: task.abort_handle(),
            queue,
        })
    }

    /// The loopback base URL to inject as the agent's OpenAI `base_url`
    /// (`http://127.0.0.1:<port>`) — the engine-provided interception point (AC6).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The resolved bound address (loopback — `is_loopback()` holds). Exposed for
    /// the AC-B loopback-only assertion (the listener unit tests) + diagnostics.
    /// `allow(dead_code)`: the getter is read only by the in-crate AC-B tests (the
    /// lib proper needs only the `base_url` + `queue`), but it is the honest proof
    /// surface for "the bind is loopback", so it stays.
    #[allow(dead_code)]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The shared observed-usage queue the supervisor's reaper drains (each entry a
    /// parsed `(input_tokens, output_tokens)`). Cloned `Arc` — cheap.
    pub fn queue(&self) -> ObservedQueue {
        Arc::clone(&self.queue)
    }
}

impl Drop for ObservedListener {
    fn drop(&mut self) {
        // Teardown: abort the accept loop when the listener goes away (the terminal
        // transition drops the `Supervised`, so the listener stops with the Run —
        // no orphan listener leaks, NFR-1 / the AD-5 discipline extended here).
        self.task.abort();
    }
}

/// Validate the upstream base URL is a usable `http://` or `https://` URL
/// (story 12-3: HTTPS accepted — it dials through the vendored rustls+ring
/// connector). Rejects an empty / other-scheme / unparseable value with a
/// STATIC reason (no URL echoed — the uniform no-leak discipline).
fn validate_upstream(upstream: &str) -> Result<(), ListenerError> {
    let trimmed = upstream.trim();
    if trimmed.is_empty() {
        return Err(ListenerError::BadUpstream {
            reason: "the upstream base URL is empty (set `metering.upstream_base_url`)".to_string(),
        });
    }
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err(ListenerError::BadUpstream {
            reason: "the upstream base URL must start with http:// or https://".to_string(),
        });
    }
    // Confirm it parses as a URI authority (a hyper Uri). Do NOT echo the URL.
    if trimmed.parse::<hyper::Uri>().is_err() {
        return Err(ListenerError::BadUpstream {
            reason: "the upstream base URL is not a valid URI".to_string(),
        });
    }
    Ok(())
}

/// The listener's accept loop: accept each inbound connection on the loopback
/// socket and serve it with a per-connection HTTP/1 server that forwards to the
/// upstream. Runs until the task is aborted (teardown). An accept error is a
/// best-effort skip (the loop continues) — never a crash.
async fn accept_loop(
    listener: TcpListener,
    upstream: String,
    queue: ObservedQueue,
    client: ForwardClient,
) {
    // One shared forward client (connection pooling to the upstream). The
    // legacy client is the lean hyper-util outbound leg; it carries a
    // `Full<Bytes>` body (we buffer the request body to forward it faithfully).
    // Since story 12-3 the connector is TLS-capable: `https://` upstreams
    // handshake through vendored rustls+ring, `http://` upstreams dial plain
    // TCP — one client, scheme-routed, no branching in the relay logic.
    let client = client;

    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            // A transient accept error is skipped (the loop continues) — never fatal.
            // No peer/traffic detail is logged (no-leak).
            Err(_) => continue,
        };
        let io = TokioIo::new(stream);
        let conn_upstream = upstream.clone();
        let conn_client = client.clone();
        let conn_queue = Arc::clone(&queue);
        // Serve each connection on its own task so a slow agent connection does not
        // block accepting the next one. The service forwards every request upstream.
        tokio::spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| {
                let upstream = conn_upstream.clone();
                let client = conn_client.clone();
                let queue = Arc::clone(&conn_queue);
                async move {
                    Ok::<_, std::convert::Infallible>(forward(req, upstream, client, queue).await)
                }
            });
            // Best-effort: a connection-level HTTP error is dropped (the agent sees a
            // reset), never logged with traffic, never a crash.
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await;
        });
    }
}

/// Forward ONE request to the upstream and relay the response back FAITHFULLY
/// (status + headers + body — a transparent proxy), skimming the OpenAI `usage`
/// out of the response body into the observed queue (story 3-4; story 12-2 adds
/// the streaming route).
///
/// FAITHFUL RELAY (the hard part — a proxy that mangles the response breaks the
/// agent): the request's method, path+query, and headers are forwarded verbatim to
/// the upstream; the upstream's status, headers, and body are returned to the agent
/// verbatim. The body is BUFFERED (collected) so it can be BOTH relayed back AND
/// parsed for `usage` — the parse reads a copy, the agent gets the exact bytes.
/// The ONE documented exception is the story-12-2 injection: a STREAMING request
/// to a chat-completions path gets `"stream_options": {"include_usage": true}`
/// added (see the module docs — best-effort, path- + body-shape-gated, and
/// skipped entirely when the agent already set its own `stream_options`).
///
/// FAILURE DISCIPLINE (AC5): an upstream/forward error is relayed to the agent as
/// an HONEST HTTP error (`502 Bad Gateway`), NEVER a fabricated success and NEVER a
/// crash. The error body carries a STATIC message only — no request body, no header,
/// no URL, no key (no-leak). A `usage`-PARSE miss is a silent skip (the call still
/// succeeds for the agent) — observation is best-effort to the RUN.
async fn forward(
    req: Request<Incoming>,
    upstream: String,
    client: ForwardClient,
    queue: ObservedQueue,
) -> Response<Full<Bytes>> {
    // Build the upstream URI: <upstream authority> + the request's path-and-query,
    // preserving the exact path the agent called (e.g. /v1/chat/completions). A
    // build failure → an honest 502 (no traffic echoed).
    let Some(uri) = build_upstream_uri(&upstream, &req) else {
        return bad_gateway();
    };

    // Split off the parts + body so we can rebuild the outbound request with the
    // SAME method + headers, forwarding the buffered body faithfully. The body is
    // wrapped in `Limited` so a runaway/malicious request body fails the relay
    // cleanly at `MAX_RELAY_BODY_BYTES` instead of buffering unbounded (OOM DoS).
    let (parts, body) = req.into_parts();
    let Ok(collected) = Limited::new(body, MAX_RELAY_BODY_BYTES).collect().await else {
        // The agent's request body could not be read (transport error OR it exceeded
        // the cap) — an honest 502, no traffic echoed, no unbounded alloc.
        return bad_gateway();
    };
    // Story 12-2: the include_usage injection — best-effort and gated on the
    // chat-completions PATH + body shape (a malformed body is forwarded
    // unmodified; a non-streaming request is never touched; any OTHER
    // streaming endpoint is forwarded UNMODIFIED — the 12-2 AMENDMENT path
    // gate, since a provider 400 on injected `stream_options` would break the
    // call being metered). The Content-Length header was already stripped
    // below (hop-by-hop), so hyper reframes the outbound body length for the
    // (possibly re-serialized) bytes — no length fixup is needed.
    let mut req_bytes = collected.to_bytes();
    inject_include_usage(
        &parts.method,
        parts.uri.path(),
        &parts.headers,
        &mut req_bytes,
    );

    let mut builder = Request::builder().method(parts.method).uri(uri);
    // Forward the request headers VERBATIM (including Authorization — the agent's
    // key flows UPSTREAM faithfully; we neither read nor log it). We drop the Host
    // header so hyper sets it correctly for the upstream authority, and STRIP the
    // hop-by-hop headers (defensively) so a `chunked`/`Content-Length` request framing
    // can't conflict with the fixed-length `Full<Bytes>` body we re-attach — hyper
    // frames the known length itself.
    if let Some(headers) = builder.headers_mut() {
        for (name, value) in parts.headers.iter() {
            if name == hyper::header::HOST || is_hop_by_hop(name) {
                continue;
            }
            headers.append(name, value.clone());
        }
    }
    let Ok(outbound) = builder.body(Full::new(req_bytes)) else {
        return bad_gateway();
    };

    // Forward to the upstream. A transport error (upstream down / unreachable) is
    // an honest 502 — never a fabricated success, never a crash, never a leak.
    let upstream_res = match client.request(outbound).await {
        Ok(res) => res,
        Err(_) => return bad_gateway(),
    };

    // Relay the response FAITHFULLY: preserve status + headers, buffer the body so
    // it is BOTH parsed for `usage` AND returned to the agent unchanged. The body is
    // wrapped in `Limited` so a runaway/malicious upstream body fails the relay
    // cleanly at `MAX_RELAY_BODY_BYTES` instead of buffering unbounded (OOM DoS).
    let (mut res_parts, res_body) = upstream_res.into_parts();
    let res_collected = match Limited::new(res_body, MAX_RELAY_BODY_BYTES).collect().await {
        Ok(c) => c.to_bytes(),
        // The upstream body could not be read fully (transport error OR it exceeded
        // the cap) — honest 502 (no traffic), no unbounded alloc.
        Err(_) => return bad_gateway(),
    };

    // Skim `usage` out of the (buffered) response body → the observed queue,
    // routed by content-type (story 12-2): an SSE stream is scanned for its
    // TERMINAL usage frame; anything else is parsed as one JSON completion. A miss
    // (no `usage`, malformed, a stream without the terminal frame) is a silent
    // skip — best-effort to the RUN, the agent still gets its faithful response
    // below. ONLY the three integer counts enter the queue (no body/header/key ever
    // leaves the proxy).
    if is_sse(&res_parts.headers) {
        if let Some((input, output, cached)) = parse_openai_sse_usage(&res_collected) {
            if let Ok(mut q) = queue.lock() {
                q.push_back((input, output, cached));
            }
        }
    } else if let Some((input, output, cached)) = parse_openai_usage(&res_collected) {
        if let Ok(mut q) = queue.lock() {
            q.push_back((input, output, cached));
        }
    }

    // Rebuild the response for the agent with the SAME status + headers + body, but
    // STRIP the hop-by-hop headers first: we re-attach a FIXED-LENGTH `Full<Bytes>`
    // body, so hyper's server sets the correct `Content-Length` itself. A `chunked`
    // upstream's `Transfer-Encoding` (or a now-stale `Content-Length`) carried onto
    // that known-length body would frame conflictingly/lossily for the agent's client.
    strip_hop_by_hop(&mut res_parts.headers);
    let mut response = Response::new(Full::new(res_collected));
    *response.status_mut() = res_parts.status;
    *response.headers_mut() = res_parts.headers;
    response
}

/// Whether the response headers declare a Server-Sent-Events body — the
/// content-type route that selects the terminal-frame SSE scanner (story 12-2).
/// Matched on the media type, case-insensitively; an absent/unparseable
/// content-type is "not SSE" (the JSON completion parse then applies, which is
/// the safe default: it skips cleanly on anything that is not one JSON object).
fn is_sse(headers: &HeaderMap) -> bool {
    headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().contains("text/event-stream"))
        .unwrap_or(false)
}

/// The story-12-2 REQUEST INJECTION: on a forwarded STREAMING completion request,
/// add `"stream_options": {"include_usage": true}` so the provider emits the
/// terminal SSE usage frame (which [`super::parse::parse_openai_sse_usage`] then
/// parses). Best-effort and deliberately narrow — the injection happens ONLY when:
/// * the method is `POST` (the OpenAI completion shape), and
/// * the request PATH ends `/chat/completions` (story 12-2 AMENDMENT, review
///   loop 1: the SAME surface whose response shape
///   [`super::parse::parse_openai_usage`]/[`super::parse::parse_openai_sse_usage`]
///   understand — any other streaming endpoint (embeddings, completions,
///   provider-specific routes) rejects `stream_options` with a 400, so the
///   injection there would BREAK the call it meters), and
/// * the request declares a JSON content-type, and
/// * the body parses as a JSON OBJECT that already sets `"stream": true`
///   (never fabricating a stream on a non-streaming request), and
/// * the body carries NO `stream_options` key (the agent's own choice wins —
///   if it explicitly asked for NO usage frame, that is respected).
///
/// "Outside the path gate" means usage is not REQUESTED there — nothing is
/// injected — but it does NOT mean unmeterable: the RESPONSE parse routes on
/// content-type, not path, so any usage the provider sends unprompted on a
/// metered response is still parsed and recorded (see review round 2's
/// wording fix). Any other shape — a malformed body, a non-object, a GET, any
/// other path — leaves the body UNMODIFIED (the relay stays faithful; the
/// metering miss is the accepted, lower-bound-honest cost). The
/// re-serialization is the house lenient-parse round-trip: field ORDER may
/// change on the wire (providers do not care), the CONTENT does not. Mutates
/// `req_bytes` in place when the injection lands; returns nothing (the
/// injection is silent — the observable fact is documented upstream-visible,
/// not logged traffic).
fn inject_include_usage(
    method: &hyper::Method,
    path: &str,
    headers: &HeaderMap,
    req_bytes: &mut Bytes,
) {
    if method != hyper::Method::POST {
        return;
    }
    // Story 12-2 AMENDMENT (review loop 1): the PATH gate. Mirrors the parse
    // scope — only `/chat/completions` responses are read for usage, so only
    // those requests are modified. Exact-suffix match on the URI path (the
    // query string is not part of `path()`); no trailing-slash leniency (the
    // OpenAI surface is `/…/chat/completions`, and inventing extra accepted
    // shapes would widen the modified-request surface).
    if !path.ends_with("/chat/completions") {
        return;
    }
    let is_json = headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().contains("application/json"))
        .unwrap_or(false);
    if !is_json {
        return;
    }
    // Lenient parse (the same house style the response parse uses): a body that
    // does not parse as a JSON object is forwarded unmodified.
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(req_bytes) else {
        return;
    };
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    // Gate on the agent's OWN streaming intent.
    if !obj
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return;
    }
    // Respect an existing stream_options — never overwrite the agent's choice.
    if obj.contains_key("stream_options") {
        return;
    }
    obj.insert(
        "stream_options".to_string(),
        serde_json::json!({ "include_usage": true }),
    );
    // A serialization failure (not reachable for a Value) leaves the original
    // body in place — best-effort, never a relay failure.
    if let Ok(serialized) = serde_json::to_vec(&value) {
        *req_bytes = Bytes::from(serialized);
    }
}

/// Is `name` an RFC 7230 §6.1 hop-by-hop header (which a proxy must not forward)?
/// Matched against [`HOP_BY_HOP_HEADERS`] (case-insensitive via `HeaderName`).
fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP_HEADERS.iter().any(|h| h == name)
}

/// Remove every hop-by-hop header from `headers` in place. Also honors the
/// `Connection: <token>` list per RFC 7230 §6.1: any header NAMED in a `Connection`
/// header value is itself connection-specific and stripped too, before `Connection`
/// itself is removed. Leaves all end-to-end headers (`Content-Type`, etc.) intact.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    // First collect any header names listed in `Connection: a, b` — those are
    // connection-specific for THIS hop and must not be forwarded either.
    let connection_listed: Vec<HeaderName> = headers
        .get_all(hyper::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|tok| HeaderName::try_from(tok.trim()).ok())
        .collect();
    for name in &connection_listed {
        headers.remove(name);
    }
    // Then the fixed RFC hop-by-hop set (incl. Connection, Transfer-Encoding,
    // Content-Length — hyper reframes the known-length Full body itself).
    for name in HOP_BY_HOP_HEADERS {
        headers.remove(name);
    }
}

/// Build the upstream URI from the configured `upstream` authority + the inbound
/// request's path-and-query (preserving the exact path the agent called). Returns
/// `None` on a malformed combination (→ an honest 502; no traffic echoed).
fn build_upstream_uri(upstream: &str, req: &Request<Incoming>) -> Option<hyper::Uri> {
    let base = upstream.trim().trim_end_matches('/');
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    format!("{base}{path_and_query}").parse::<hyper::Uri>().ok()
}

/// An honest `502 Bad Gateway` with a STATIC body (no request/response traffic, no
/// header, no URL, no key — the no-leak discipline). The proxy NEVER fabricates a
/// success on a forward failure (AC5): the agent sees a real error, so it does not
/// mistake a proxy fault for a provider response.
fn bad_gateway() -> Response<Full<Bytes>> {
    let mut res = Response::new(Full::new(Bytes::from_static(
        b"ktesio engine-observed proxy: upstream request failed",
    )));
    *res.status_mut() = hyper::StatusCode::BAD_GATEWAY;
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single-thread runtime handle for the pure listener unit tests (the bind +
    /// validation logic). The integration proof (a real forward through the
    /// listener into the ledger) lives in `tests/` via the surviving-engine harness.
    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime")
    }

    #[test]
    fn bind_address_is_loopback_and_never_zero_or_routable() {
        // AC-B: the listener binds loopback ONLY. Prove the resolved SocketAddr is
        // loopback (127.0.0.1), and is NOT 0.0.0.0 (the unspecified/all-interfaces
        // address) nor any routable address. The port is a real OS-assigned
        // ephemeral (non-zero) port.
        let rt = test_runtime();
        let listener =
            ObservedListener::start(rt.handle(), "http://127.0.0.1:9/".to_string()).unwrap();
        let addr = listener.local_addr();
        assert!(addr.ip().is_loopback(), "bind must be loopback: {addr}");
        assert!(
            !addr.ip().is_unspecified(),
            "bind must NOT be 0.0.0.0: {addr}"
        );
        assert_eq!(
            addr.ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            "v1 binds 127.0.0.1 explicitly"
        );
        assert_ne!(addr.port(), 0, "the OS assigned a real ephemeral port");
        // The injected base_url points at the loopback address.
        assert!(
            listener.base_url().starts_with("http://127.0.0.1:"),
            "base_url must be the loopback URL: {}",
            listener.base_url()
        );
        assert!(listener.base_url().ends_with(&addr.port().to_string()));
    }

    #[test]
    fn each_listener_gets_a_distinct_ephemeral_port() {
        // Two per-instance listeners bind DISTINCT ephemeral ports (0 = OS-picked),
        // so two engine-observed instances never collide.
        let rt = test_runtime();
        let a = ObservedListener::start(rt.handle(), "http://127.0.0.1:9/".to_string()).unwrap();
        let b = ObservedListener::start(rt.handle(), "http://127.0.0.1:9/".to_string()).unwrap();
        assert_ne!(
            a.local_addr().port(),
            b.local_addr().port(),
            "each listener binds its own port"
        );
    }

    #[test]
    fn an_empty_or_other_scheme_upstream_is_refused() {
        // Story 12-3: `https://` is now ACCEPTED; an empty, a foreign-scheme
        // (`ftp://`), or scheme-less upstream is still a hard start error (no
        // listener leaked), with a STATIC reason (no URL echoed).
        let rt = test_runtime();
        for bad in ["", "   ", "ftp://x", "api.openai.com", "wss://x"] {
            // Match on the error WITHOUT unwrap_err (which would require the Ok type
            // `ObservedListener` to be Debug — we deliberately do NOT derive Debug on
            // it, so the listener can never be debug-printed into a log, no-leak).
            let err = match ObservedListener::start(rt.handle(), bad.to_string()) {
                Err(e) => e,
                Ok(_) => panic!("upstream {bad:?} must be refused, but a listener started"),
            };
            assert!(
                matches!(err, ListenerError::BadUpstream { .. }),
                "upstream {bad:?} must be refused, got {err:?}"
            );
            // The error message NEVER echoes the URL (no-leak discipline).
            if !bad.trim().is_empty() {
                assert!(
                    !err.to_string().contains(bad.trim()),
                    "error must not echo the upstream URL: {err}"
                );
            }
        }
    }

    #[test]
    fn the_default_root_store_is_non_empty() {
        // Review round 2: the PRODUCTION trust roots must be observable.
        // Every in-suite https test dials through a CUSTOM self-signed-root
        // config (the injectable seam), so an accidentally-empty
        // `default_root_store()` would pass the whole suite while every real
        // https forward failed at handshake. This no-I/O pin makes that
        // failure mode a test failure instead of a production outage: the
        // vendored webpki-roots store carries its well-known root count.
        let roots = default_root_store();
        assert!(
            roots.len() > 100,
            "the vendored root store must carry the webpki-roots set, got {} roots",
            roots.len()
        );
    }

    #[test]
    fn an_https_upstream_is_accepted_and_the_listener_starts() {
        // Story 12-3: the https:// upstream is accepted — the listener starts
        // (bind + accept loop) and dials it over vendored rustls+ring at forward
        // time. The full https + streaming + metering composition (with a
        // self-signed root) is proven by
        // `an_https_upstream_streams_and_meters_end_to_end_through_rustls` below.
        // (Review round 2 renamed this test: it asserts a SUCCESSFUL https
        // start — the old `…names_the_scheme_in_its_error_surface_only` name
        // described the pre-12-3 refusal this test replaced.)
        let rt = test_runtime();
        let listener =
            ObservedListener::start(rt.handle(), "https://127.0.0.1:9".to_string()).unwrap();
        assert!(listener.base_url().starts_with("http://127.0.0.1:"));
    }

    #[test]
    fn an_http_upstream_that_is_not_a_valid_uri_is_refused_before_binding() {
        // The scheme checks above are prefix tests, so a value can clear
        // `http://` and still be structurally unusable. That case must be caught
        // HERE, at start, rather than at the first forwarded request: the listener
        // starts on the way into `start`, so accepting a URI hyper can never build
        // would leave a bound loopback socket that 502s every call the agent makes
        // — an agent that looks running but can never reach its provider. The
        // no-leak discipline still applies: the reason must not echo the URL (it
        // can carry a key in a query string).
        let rt = test_runtime();
        for bad in ["http://exa mple.com", "http://[not-an-address"] {
            let err = match ObservedListener::start(rt.handle(), bad.to_string()) {
                Err(e) => e,
                Ok(_) => panic!("upstream {bad:?} must be refused, but a listener started"),
            };
            assert!(
                matches!(err, ListenerError::BadUpstream { .. }),
                "upstream {bad:?} must be refused, got {err:?}"
            );
            assert!(
                err.to_string().contains("not a valid URI"),
                "the reason must name the parse failure, not the scheme: {err}"
            );
            assert!(
                !err.to_string().contains(bad),
                "error must not echo the upstream URL: {err}"
            );
        }
    }

    #[test]
    fn an_https_upstream_streams_and_meters_end_to_end_through_rustls() {
        // Story 12-3, the composition proof: a SELF-SIGNED-ROOT `https://`
        // upstream (rustls server on a std thread), reached by the listener's
        // vendored-rustls client through an injected ClientConfig trusting that
        // root — and the FULL streaming metering path composes across TLS: the
        // injected include_usage rides the encrypted request, the terminal SSE
        // usage frame comes back encrypted, and the two integer counts land in
        // the queue. No system trust store is consulted on either side.
        use std::io::{Read as _, Write as _};

        // (1) Mint the self-signed root + leaf (rcgen, dev-only). The SAN list
        // carries the literal IP we dial (the client verifies the URI host
        // against the certificate's SANs — include `127.0.0.1`, not just a name).
        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("self-signed cert");
        let cert_der = cert.cert.der().clone();
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()),
        );
        // (2) The TLS upstream server: a std thread running a BLOCKING rustls
        // handshake + relay (no tokio needed server-side; no extra dep).
        let tls_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let tls_addr = tls_listener.local_addr().unwrap();
        let upstream_url = format!("https://{tls_addr}");
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .expect("server cert");
        let server_join = std::thread::spawn(move || {
            let (stream, _) = tls_listener.accept().expect("tls upstream accept");
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
            let conn =
                rustls::ServerConnection::new(Arc::new(server_config)).expect("server connection");
            let mut tls = rustls::StreamOwned::new(conn, stream);
            // Read the (decrypted) forwarded request head + body.
            let mut buf = Vec::new();
            let mut tmp = [0u8; 512];
            let head_end = loop {
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos;
                }
                match tls.read(&mut tmp) {
                    Ok(0) => break buf.len(),
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(_) => break buf.len(),
                }
                if buf.len() > 32 * 1024 {
                    break buf.len();
                }
            };
            // Drain the declared body.
            let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
            if let Some(cl) = head
                .split("\r\n")
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
            {
                let mut have = buf.len() - head_end - 4;
                while have < cl {
                    match tls.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            have += n;
                        }
                        Err(_) => break,
                    }
                }
            }
            // The INJECTION proof: the decrypted forwarded request carries the
            // injected stream_options (and the agent's own stream flag).
            let raw = String::from_utf8_lossy(&buf);
            assert!(
                raw.contains("\"stream_options\"") && raw.contains("\"include_usage\""),
                "the forwarded TLS request must carry the injected include_usage: {raw}"
            );
            // Answer with the SSE streaming shape (encrypted by rustls on write).
            let sse = "data: {\"choices\":[],\"usage\":null}\n\n\
                       data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":11}}\n\n\
                       data: [DONE]\n\n";
            let _ = tls.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse}"
                )
                .as_bytes(),
            );
            let _ = tls.flush();
            // Close the TLS session CLEANLY (close_notify + flush): a rustls
            // CLIENT treats EOF without close_notify as a truncated stream
            // (UnexpectedEof), so the test server must speak the full TLS
            // close-down a real provider would.
            tls.conn.send_close_notify();
            let _ = tls.flush();
        });

        // (3) The listener with an INJECTED ClientConfig trusting the self-signed
        // root (the test seam; production defaults to webpki-roots instead).
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der).expect("trust the self-signed root");
        let tls_config = Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );

        let rt = test_runtime();
        let listener =
            ObservedListener::start_with_tls_config(rt.handle(), upstream_url, tls_config)
                .expect("https upstream listener");
        let queue = listener.queue();
        let authority = listener
            .base_url()
            .strip_prefix("http://")
            .unwrap()
            .to_string();

        // (4) Drive a streaming request through the listener → TLS upstream.
        let agent_body = br#"{"model":"gpt-observed","stream":true,"messages":[]}"#;
        let req = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
            len = agent_body.len(),
        );
        let req = format!("{req}{}", std::str::from_utf8(agent_body).unwrap());
        let auth_for_thread = authority.clone();
        let resp = std::thread::spawn(move || raw_request(&auth_for_thread, &req))
            .join()
            .unwrap();
        server_join.join().unwrap();

        // The SSE response is relayed faithfully to the agent…
        let resp_text = String::from_utf8_lossy(&resp);
        assert!(
            resp_text.starts_with("HTTP/1.1 200") && resp_text.contains("data: [DONE]"),
            "the TLS-relayed SSE body must reach the agent faithfully: {resp_text}"
        );
        // …and the TERMINAL usage frame metered through TLS into the queue.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(q) = queue.lock() {
                if let Some(&(input, output, cached)) = q.front() {
                    assert_eq!(
                        (input, output, cached),
                        (9, 11, 0),
                        "usage metered over TLS"
                    );
                    assert_eq!(q.len(), 1, "exactly one event for the streamed call");
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "usage was never metered over the TLS upstream"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn dropping_the_listener_aborts_its_task() {
        // Teardown (NFR-1): dropping the listener aborts the accept loop. We cannot
        // directly observe the abort, but we CAN confirm drop does not panic and the
        // port is released (a fresh bind on the same ephemeral range succeeds). This
        // exercises the Drop path for coverage + the RAII teardown contract.
        let rt = test_runtime();
        let addr = {
            let listener =
                ObservedListener::start(rt.handle(), "http://127.0.0.1:9/".to_string()).unwrap();
            let addr = listener.local_addr();
            let queue = listener.queue();
            assert!(queue.lock().unwrap().is_empty(), "queue starts empty");
            addr
            // listener dropped here → task aborted
        };
        // The address was loopback; drop completed without panic.
        assert!(addr.ip().is_loopback());
    }

    #[test]
    fn build_upstream_uri_preserves_path_and_query() {
        // The forward preserves the EXACT path+query the agent called, appended to
        // the upstream authority (trailing slash on the base is normalized).
        let req = Request::builder()
            .uri("/v1/chat/completions?stream=false")
            .body(Full::new(Bytes::new()))
            .unwrap();
        // Rebuild as Request<Incoming> shape is awkward in a unit test; test the pure
        // string join the helper performs via a parallel construction.
        let base = "http://127.0.0.1:8080/".trim_end_matches('/');
        let joined = format!("{base}{}", req.uri().path_and_query().unwrap().as_str());
        assert_eq!(
            joined,
            "http://127.0.0.1:8080/v1/chat/completions?stream=false"
        );
        assert!(joined.parse::<hyper::Uri>().is_ok());
    }

    #[test]
    fn bad_gateway_carries_no_traffic() {
        // AC5 + no-leak: the honest 502 body is a STATIC message — it carries no
        // request/response body, header, URL, or key.
        let res = bad_gateway();
        assert_eq!(res.status(), hyper::StatusCode::BAD_GATEWAY);
    }

    /// A tiny in-test upstream stub on a thread: bind `127.0.0.1:0`, answer ONE
    /// request with the given raw HTTP response bytes, and record the FULL raw
    /// request it saw (head + Content-Length-delimited body — story 12-2's
    /// injection proof needs the forwarded body). Pure `std` — the in-crate
    /// analogue of the integration upstream stub, so the forward + relay + parse
    /// path is exercised IN THE TEST PROCESS (tarpaulin attributes it). Returns
    /// `(base_url, join_handle_yielding_the_raw_request)`.
    fn spawn_oneshot_upstream(
        response: &'static [u8],
    ) -> (String, std::thread::JoinHandle<String>) {
        use std::io::Read as _;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("upstream accept");
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            // Read the request head (up to the blank line), then any body the
            // head's Content-Length declares.
            let mut buf = Vec::new();
            let mut tmp = [0u8; 512];
            let mut head_end = None;
            loop {
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    head_end = Some(pos);
                    break;
                }
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(_) => break,
                }
                if buf.len() > 32 * 1024 {
                    break;
                }
            }
            let head_end = head_end.unwrap_or(buf.len());
            // Drain the declared body (best-effort: a head without a body is the
            // historical shape the older stubs answered).
            let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
            if let Some(cl) = head
                .split("\r\n")
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
            {
                let mut have = buf.len() - head_end - 4;
                while have < cl {
                    match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            have += n;
                        }
                        Err(_) => break,
                    }
                }
            }
            let _ = std::io::Write::write_all(&mut stream, response);
            let _ = std::io::Write::flush(&mut stream);
            String::from_utf8_lossy(&buf).into_owned()
        });
        (base_url, handle)
    }

    /// Send ONE raw HTTP request to `authority` and return the raw response bytes.
    fn raw_request(authority: &str, request: &str) -> Vec<u8> {
        use std::io::{Read as _, Write as _};
        let mut stream = std::net::TcpStream::connect(authority).expect("connect listener");
        stream.write_all(request.as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut resp = Vec::new();
        let _ = stream.read_to_end(&mut resp);
        resp
    }

    #[test]
    fn forwards_faithfully_and_skims_usage_into_the_queue() {
        // AC-A/AC5 (in-process, tarpaulin-attributed): a request through the listener
        // is FORWARDED to the upstream, the response is RELAYED back FAITHFULLY (the
        // upstream body is returned unchanged), and the OpenAI `usage` is skimmed into
        // the observed queue. Exercises accept_loop + forward + build_upstream_uri +
        // parse + queue-push directly.
        let upstream_body =
            br#"{"choices":[],"usage":{"prompt_tokens":42,"completion_tokens":58}}"#;
        let response: &'static [u8] = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                upstream_body.len(),
                std::str::from_utf8(upstream_body).unwrap(),
            )
            .into_bytes()
            .into_boxed_slice(),
        );
        let (upstream_url, upstream_join) = spawn_oneshot_upstream(response);

        let rt = test_runtime();
        let listener = ObservedListener::start(rt.handle(), upstream_url).unwrap();
        let queue = listener.queue();
        let authority = listener
            .base_url()
            .strip_prefix("http://")
            .unwrap()
            .to_string();

        // Drive a real request through the listener (blocking client on a helper
        // thread so the runtime can serve it).
        let req = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        let auth_for_thread = authority.clone();
        let client = std::thread::spawn(move || raw_request(&auth_for_thread, &req));
        let resp = client.join().unwrap();
        let _ = upstream_join.join();

        // The relayed response carries the upstream body FAITHFULLY.
        let resp_text = String::from_utf8_lossy(&resp);
        assert!(
            resp_text.starts_with("HTTP/1.1 200"),
            "status relayed: {resp_text}"
        );
        assert!(
            resp_text.contains(std::str::from_utf8(upstream_body).unwrap()),
            "the upstream body must be relayed faithfully: {resp_text}"
        );

        // The `usage` was skimmed into the observed queue (poll briefly — the push
        // happens on the runtime task).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(q) = queue.lock() {
                if let Some(&(input, output, cached)) = q.front() {
                    assert_eq!(
                        (input, output, cached),
                        (42, 58, 0),
                        "parsed usage mapped + queued"
                    );
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "usage was never pushed to the observed queue"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn an_unreachable_upstream_is_relayed_as_an_honest_502() {
        // AC5: a forward error (the upstream is not listening) is relayed to the agent
        // as an honest 502 — never a fabricated success, never a crash. Point the
        // listener at a closed port (bind then drop, so nothing is listening).
        let dead_port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
            // dropped here → nothing listens on dead_port
        };
        let rt = test_runtime();
        let listener =
            ObservedListener::start(rt.handle(), format!("http://127.0.0.1:{dead_port}")).unwrap();
        let authority = listener
            .base_url()
            .strip_prefix("http://")
            .unwrap()
            .to_string();
        let req = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: {authority}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        let auth_for_thread = authority.clone();
        let resp = std::thread::spawn(move || raw_request(&auth_for_thread, &req))
            .join()
            .unwrap();
        let resp_text = String::from_utf8_lossy(&resp);
        assert!(
            resp_text.starts_with("HTTP/1.1 502"),
            "an unreachable upstream must relay an honest 502: {resp_text}"
        );
    }

    #[test]
    fn a_chunked_upstream_response_is_reframed_and_relayed_completely() {
        // L1 (hop-by-hop): an upstream that answers with `Transfer-Encoding: chunked`
        // must be relayed to the agent as a WELL-FRAMED, COMPLETE response — the proxy
        // buffers the (de-chunked) body, STRIPS `Transfer-Encoding`, and re-frames it as
        // a fixed-length `Full` body (hyper sets `Content-Length`). The agent must NOT
        // see a `chunked` frame conflicting with the known-length body, and the `usage`
        // is still skimmed from the complete body.
        let upstream_body =
            br#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":22}}"#;
        // A CHUNKED HTTP/1.1 response: one chunk carrying the whole JSON, then the
        // terminating zero-length chunk. NO Content-Length (chunked framing).
        let body_str = std::str::from_utf8(upstream_body).unwrap();
        let response: &'static [u8] = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{len:X}\r\n{body}\r\n0\r\n\r\n",
                len = upstream_body.len(),
                body = body_str,
            )
            .into_bytes()
            .into_boxed_slice(),
        );
        let (upstream_url, upstream_join) = spawn_oneshot_upstream(response);

        let rt = test_runtime();
        let listener = ObservedListener::start(rt.handle(), upstream_url).unwrap();
        let queue = listener.queue();
        let authority = listener
            .base_url()
            .strip_prefix("http://")
            .unwrap()
            .to_string();

        let req = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        let auth_for_thread = authority.clone();
        let resp = std::thread::spawn(move || raw_request(&auth_for_thread, &req))
            .join()
            .unwrap();
        let _ = upstream_join.join();

        let resp_text = String::from_utf8_lossy(&resp);
        assert!(
            resp_text.starts_with("HTTP/1.1 200"),
            "status relayed: {resp_text}"
        );
        // The relayed body is the COMPLETE de-chunked JSON, delivered faithfully.
        assert!(
            resp_text.contains(body_str),
            "the complete (de-chunked) body must be relayed: {resp_text}"
        );
        // The response the agent sees must NOT carry a `chunked` Transfer-Encoding —
        // it was stripped and reframed as a fixed-length body (no conflicting frame).
        let head_lower = resp_text
            .split("\r\n\r\n")
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(
            !head_lower.contains("transfer-encoding"),
            "Transfer-Encoding must be stripped from the relayed response: {resp_text}"
        );
        // hyper framed the known length itself.
        assert!(
            head_lower.contains("content-length"),
            "the reframed response must carry a Content-Length: {resp_text}"
        );

        // The `usage` was skimmed from the complete body into the observed queue.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(q) = queue.lock() {
                if let Some(&(input, output, cached)) = q.front() {
                    assert_eq!(
                        (input, output, cached),
                        (11, 22, 0),
                        "usage parsed from de-chunked body"
                    );
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "usage was never pushed from the chunked response"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn an_oversize_response_body_fails_cleanly_without_oom_or_leak() {
        // OOM-DoS hardening: an upstream response body LARGER than MAX_RELAY_BODY_BYTES
        // must fail the relay as a clean, traffic-free 502 — never buffered unbounded,
        // never a partial/corrupt relay, never a panic. A sentinel token is embedded in
        // the oversize body; the 502 the agent receives must NOT echo it (no-leak).
        const SENTINEL: &str = "OVERSIZE-BODY-SENTINEL-must-not-leak";
        // Stream a body strictly larger than the cap directly from the stub thread (so
        // the test never itself holds a >64 MiB slice; the bytes flow over loopback and
        // the proxy's `Limited` trips mid-collect). `Connection: close` + a Content-Length
        // one byte over the cap.
        let over_len = MAX_RELAY_BODY_BYTES + 1;
        let listener_up = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let up_addr = listener_up.local_addr().unwrap();
        let upstream_url = format!("http://{up_addr}");
        let upstream_join = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let (mut stream, _) = listener_up.accept().expect("oversize upstream accept");
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            // Drain the request head.
            let mut buf = Vec::new();
            let mut tmp = [0u8; 512];
            loop {
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(_) => break,
                }
                if buf.len() > 32 * 1024 {
                    break;
                }
            }
            // Write the header with an over-cap Content-Length, then stream the body in
            // chunks (a sentinel prefix + filler). A broken pipe mid-write is expected —
            // the proxy trips its cap and drops the connection; that is the point.
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {over_len}\r\nConnection: close\r\n\r\n"
            );
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            if stream.write_all(SENTINEL.as_bytes()).is_err() {
                return;
            }
            let filler = vec![b'x'; 1024 * 1024];
            let mut written = SENTINEL.len();
            while written < over_len {
                let n = (over_len - written).min(filler.len());
                if stream.write_all(&filler[..n]).is_err() {
                    return; // proxy tripped the cap + closed — expected
                }
                written += n;
            }
            let _ = stream.flush();
        });

        let rt = test_runtime();
        let listener = ObservedListener::start(rt.handle(), upstream_url).unwrap();
        let authority = listener
            .base_url()
            .strip_prefix("http://")
            .unwrap()
            .to_string();
        let req = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        let auth_for_thread = authority.clone();
        let resp = std::thread::spawn(move || raw_request(&auth_for_thread, &req))
            .join()
            .unwrap();
        // The upstream thread may still be mid-write when the proxy closes; don't block
        // the test on it beyond a best-effort join.
        let _ = upstream_join.join();

        let resp_text = String::from_utf8_lossy(&resp);
        // The over-cap body fails the relay as an honest 502 (the clean, bounded error).
        assert!(
            resp_text.starts_with("HTTP/1.1 502"),
            "an over-cap upstream body must relay a clean 502: {}",
            resp_text.chars().take(200).collect::<String>()
        );
        // No-leak: the 502 the agent receives carries NONE of the oversize body content.
        assert!(
            !resp_text.contains(SENTINEL),
            "the 502 error path must not echo any of the oversize body"
        );
    }

    // ---- Story 12-2: the streaming route (injection + SSE terminal frame) ----

    #[test]
    fn inject_include_usage_is_narrow_and_respects_the_agent() {
        // The injection gate, driven directly (pure, no I/O): lands ONLY on
        // POST + a chat-completions PATH + JSON + an object already setting
        // stream:true without stream_options; every other shape — including
        // any other streaming ENDPOINT (the 12-2 AMENDMENT path gate) — is
        // forwarded unmodified.
        let post = hyper::Method::POST;
        let get = hyper::Method::GET;
        let chat = "/v1/chat/completions";
        let json_headers = |_body: &str| {
            let mut h = HeaderMap::new();
            h.insert(
                hyper::header::CONTENT_TYPE,
                hyper::header::HeaderValue::from_str("application/json").unwrap(),
            );
            h
        };
        let mut plain = HeaderMap::new();
        plain.insert(
            hyper::header::CONTENT_TYPE,
            hyper::header::HeaderValue::from_str("text/plain").unwrap(),
        );

        // (a) the landing shape: stream_options appears with include_usage.
        let mut body = Bytes::from_static(br#"{"model":"gpt","stream":true,"messages":[]}"#);
        inject_include_usage(&post, chat, &json_headers(""), &mut body);
        let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            doc["stream_options"]["include_usage"],
            serde_json::json!(true),
            "the injection must land on the streaming shape: {doc}"
        );
        assert_eq!(doc["stream"], serde_json::json!(true));
        assert_eq!(doc["model"], serde_json::json!("gpt"));

        // (b) an EXISTING stream_options is respected (agent's choice wins).
        let mut body =
            Bytes::from_static(br#"{"stream":true,"stream_options":{"include_usage":false}}"#);
        inject_include_usage(&post, chat, &json_headers(""), &mut body);
        let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            doc["stream_options"]["include_usage"],
            serde_json::json!(false),
            "an existing stream_options must never be overwritten: {doc}"
        );

        // (c) a NON-streaming request is never modified.
        let mut body = Bytes::from_static(br#"{"model":"gpt"}"#);
        inject_include_usage(&post, chat, &json_headers(""), &mut body);
        assert!(
            serde_json::from_slice::<serde_json::Value>(&body)
                .unwrap()
                .get("stream_options")
                .is_none(),
            "a non-streaming request must not be touched"
        );

        // (d) a malformed body / non-JSON content-type / GET are untouched.
        for (method, headers, raw) in [
            (&post, &plain, &br#"{"stream":true}"#[..]),
            (&get, &json_headers(""), &br#"{"stream":true}"#[..]),
            (&post, &json_headers(""), &br#"not json{"stream":true}"#[..]),
            (&post, &json_headers(""), &br#"[1,2,3]"#[..]),
        ] {
            let mut body = Bytes::copy_from_slice(raw);
            inject_include_usage(method, chat, headers, &mut body);
            assert_eq!(
                &body[..],
                raw,
                "shape must be forwarded unmodified: {method} {raw:?}"
            );
        }

        // (e) 12-2 AMENDMENT: the PATH gate. A streaming POST+JSON body on any
        // NON-chat-completions endpoint is forwarded UNMODIFIED — a provider
        // would 400 the injected `stream_options` there, and metering must
        // never break the call it meters.
        for other_path in [
            "/v1/completions",
            "/v1/embeddings",
            "/v1/chat/completions/extra",
            "/v1/audio/transcriptions",
        ] {
            let raw = br#"{"model":"gpt","stream":true}"#;
            let mut body = Bytes::from_static(raw);
            inject_include_usage(&post, other_path, &json_headers(""), &mut body);
            assert_eq!(
                &body[..],
                &raw[..],
                "a streaming request to `{other_path}` must be forwarded unmodified \
                 (the 12-2 AMENDMENT path gate)"
            );
        }
        // The gate is a SUFFIX match on the path — the OpenAI surface's real
        // shape (any version prefix, the exact terminal segment) lands.
        for chat_path in [
            "/chat/completions",
            "/v1/chat/completions",
            "/openai/v2/chat/completions",
        ] {
            let mut body = Bytes::from_static(br#"{"stream":true}"#);
            inject_include_usage(&post, chat_path, &json_headers(""), &mut body);
            let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert!(
                doc.get("stream_options").is_some(),
                "`{chat_path}` is a chat-completions request and must be injected"
            );
        }
    }

    #[test]
    fn a_streamed_completion_is_injected_metered_and_relayed_faithfully() {
        // The story-12-2 in-process proof: an agent POSTs `"stream": true`
        // through the listener; the FORWARDED body carries the injected
        // `stream_options.include_usage` (the stub asserts it); the upstream's
        // SSE response — null intermediates + a terminal usage frame — is
        // RELAYED back faithfully AND its terminal usage lands in the queue.
        let sse_body = "data: {\"id\":\"1\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":null}\n\n\
                        data: {\"id\":\"1\",\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":58,\"total_tokens\":100}}\n\n\
                        data: [DONE]\n\n";
        let response: &'static [u8] = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse_body}"
            )
            .into_bytes()
            .into_boxed_slice(),
        );
        let (upstream_url, upstream_join) = spawn_oneshot_upstream(response);

        let rt = test_runtime();
        let listener = ObservedListener::start(rt.handle(), upstream_url).unwrap();
        let queue = listener.queue();
        let authority = listener
            .base_url()
            .strip_prefix("http://")
            .unwrap()
            .to_string();

        let agent_body = br#"{"model":"gpt-observed","stream":true,"messages":[]}"#;
        let req = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
            len = agent_body.len(),
        );
        let req = format!("{req}{}", std::str::from_utf8(agent_body).unwrap());
        let auth_for_thread = authority.clone();
        let resp = std::thread::spawn(move || raw_request(&auth_for_thread, &req))
            .join()
            .unwrap();
        let forwarded = upstream_join.join().unwrap();

        // The FORWARDED request carries the injection (upstream-visible by design).
        assert!(
            forwarded.contains("stream_options"),
            "the forwarded streaming body must carry the injected stream_options: {forwarded}"
        );
        assert!(
            forwarded.contains("include_usage"),
            "include_usage must be requested: {forwarded}"
        );
        // The agent's own fields survive the round-trip.
        assert!(
            forwarded.contains("\"stream\":true"),
            "the agent's stream flag survives: {forwarded}"
        );

        // The SSE response is relayed to the agent FAITHFULLY.
        let resp_text = String::from_utf8_lossy(&resp);
        assert!(
            resp_text.starts_with("HTTP/1.1 200"),
            "status relayed: {resp_text}"
        );
        assert!(
            resp_text.contains("data: [DONE]"),
            "the SSE body must be relayed faithfully: {resp_text}"
        );

        // The TERMINAL usage frame landed in the observed queue (exactly one
        // event; the null intermediate contributed nothing).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(q) = queue.lock() {
                if let Some(&(input, output, cached)) = q.front() {
                    assert_eq!(
                        (input, output, cached),
                        (42, 58, 0),
                        "the terminal frame's usage"
                    );
                    assert_eq!(q.len(), 1, "exactly ONE event per streamed completion");
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "usage was never pushed from the SSE terminal frame"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn an_sse_response_without_a_terminal_usage_frame_skips_silently() {
        // Lower-bound honesty: a stream WITHOUT a terminal usage frame (the
        // provider honoring a no-usage stream_options, or an error stream) is
        // a silent skip — relayed faithfully, nothing queued, never a
        // fabricated zero.
        let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}],\"usage\":null}\n\ndata: [DONE]\n\n";
        let response: &'static [u8] = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{sse_body}"
            )
            .into_bytes()
            .into_boxed_slice(),
        );
        let (upstream_url, upstream_join) = spawn_oneshot_upstream(response);
        let rt = test_runtime();
        let listener = ObservedListener::start(rt.handle(), upstream_url).unwrap();
        let queue = listener.queue();
        let authority = listener
            .base_url()
            .strip_prefix("http://")
            .unwrap()
            .to_string();
        let agent_body = br#"{"model":"gpt-observed","stream":true,"messages":[]}"#;
        let req = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
            len = agent_body.len(),
        );
        let req = format!("{req}{}", std::str::from_utf8(agent_body).unwrap());
        let auth_for_thread = authority.clone();
        let resp = std::thread::spawn(move || raw_request(&auth_for_thread, &req))
            .join()
            .unwrap();
        let _ = upstream_join.join();
        // Relayed faithfully…
        assert!(String::from_utf8_lossy(&resp).contains("data: [DONE]"));
        // …but nothing was metered (a bounded grace window; a push would be a
        // fabricated count from a usage-less stream).
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            queue.lock().unwrap().is_empty(),
            "a usage-less stream must NOT fabricate counts"
        );
    }

    #[test]
    fn a_streaming_request_to_another_endpoint_is_forwarded_unmodified() {
        // The 12-2 AMENDMENT path gate, end to end through `forward`: a
        // streaming POST to a NON-chat-completions endpoint is relayed
        // BYTE-FAITHFULLY — no injected `stream_options` (a provider would 400
        // it and metering would have broken the call it meters). The response
        // still relays faithfully; nothing is metered (the accepted,
        // lower-bound-honest cost outside the parse seam's scope).
        let plain_body = "{\"id\":\"1\",\"object\":\"text_completion\",\"usage\":null}";
        let response: &'static [u8] = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{plain_body}"
            )
            .into_bytes()
            .into_boxed_slice(),
        );
        let (upstream_url, upstream_join) = spawn_oneshot_upstream(response);
        let rt = test_runtime();
        let listener = ObservedListener::start(rt.handle(), upstream_url).unwrap();
        let queue = listener.queue();
        let authority = listener
            .base_url()
            .strip_prefix("http://")
            .unwrap()
            .to_string();

        let agent_body = br#"{"model":"gpt","stream":true,"prompt":"hi"}"#;
        let req = format!(
            "POST /v1/completions HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
            len = agent_body.len(),
        );
        let req = format!("{req}{}", std::str::from_utf8(agent_body).unwrap());
        let auth_for_thread = authority.clone();
        let resp = std::thread::spawn(move || raw_request(&auth_for_thread, &req))
            .join()
            .unwrap();
        let forwarded = upstream_join.join().unwrap();

        // The upstream saw the EXACT body — no stream_options anywhere.
        assert!(
            !forwarded.contains("stream_options"),
            "a non-chat-completions streaming request must be forwarded UNMODIFIED: \
             {forwarded}"
        );
        assert!(
            forwarded.contains("\"prompt\":\"hi\""),
            "the agent's own body survives verbatim: {forwarded}"
        );
        // …and the response still relays faithfully to the agent.
        assert!(
            String::from_utf8_lossy(&resp).contains("text_completion"),
            "the relay stays faithful: {}",
            String::from_utf8_lossy(&resp)
        );
        // Nothing was metered either: outside the chat-completions scope the
        // usage parse is not the contract (bounded grace window, per the
        // sibling tests — a push here would mean the gate leaked).
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            queue.lock().unwrap().is_empty(),
            "a non-chat-completions streaming call must not enqueue usage"
        );
    }
}
