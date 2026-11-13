//! The shared Metering Proxy (story 23-1, spine AD-26): one loopback
//! listener per data-path owner that replaces every per-instance listener.
//!
//! * [`identify`] — the pure per-call identification over a snapshot;
//! * [`wire`] — every `proxy-wire v1` response, header and strip rule;
//! * [`forward`] — the call state machine;
//! * [`records`] — call ids, the permit pool and the record pump.
//!
//! The listener binds `127.0.0.1` only (a resolved non-loopback address is
//! a start error; there is no flag to widen it). Connections are hyper
//! HTTP/1 with a 30 s header-read timeout and a keep-alive idle timeout;
//! calls are capped at [`MeteringProxyOptions::max_concurrent_calls`] and
//! the buffered bytes at [`MeteringProxyOptions::max_buffered_bytes`].
//! Graceful stop: stop accepting → in-flight calls finish within the grace
//! → the rest are aborted (`shutdown`) → the record pump closes.

#[cfg(test)]
#[path = "custody/tests.rs"]
mod custody_tests;
pub(crate) mod forward;
pub(crate) mod hold;
pub(crate) mod identify;
pub(crate) mod records;
#[cfg(test)]
mod tests;
pub(crate) mod wire;

use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::domain::pipeline::EvaluationInbox;
use crate::domain::runs::snapshot::SnapshotPublisher;
use crate::store::writer::WriterHandle;

/// The proxy's open-time options (story 23-1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeteringProxyOptions {
    /// Run the proxy (default `true`). A read-only engine never runs one.
    pub enabled: bool,
    /// The loopback port (`0` = an OS pick).
    pub port: u16,
    /// Record the bound port for the state directory and reuse it (the
    /// host sets this; embedders default to an unrecorded ephemeral port).
    pub sticky: bool,
    /// Add `x-hekma-run-id` and `x-hekma-labels` to outbound calls.
    pub identity_headers: bool,
    /// The most calls in flight at once.
    pub max_concurrent_calls: usize,
    /// The most request and response bytes buffered at once.
    pub max_buffered_bytes: u64,
    /// Host header values beyond the loopback literals the Host/Origin
    /// guard accepts (story 22-2, `--proxy-allowed-hosts`; empty = the
    /// loopback literals alone).
    pub allowed_hosts: Vec<String>,
    /// `Origin` header values the Host/Origin guard accepts (story 22-2,
    /// `--proxy-allowed-origins`; empty = every `Origin` is refused —
    /// SDKs send none, and a browser page's cross-site request is the
    /// threat).
    pub allowed_origins: Vec<String>,
    /// Story 23-2: the Lease and window knobs (the sweeper runs beside the
    /// proxy, in the engine that owns the data path).
    pub leases: crate::domain::runs::lease::LeaseOptions,
    /// Story 23-4: the most calls one held Run may have parked; past it a
    /// call gets the retryable hold-full refusal.
    pub max_held_per_run: usize,
    /// cfg(test) seam: one extra trusted root (DER) for the custody tests'
    /// TLS fake upstream.
    #[cfg(test)]
    pub(crate) test_root_der: Option<Vec<u8>>,
}

/// The upstream client's TLS config (the cfg(test) seam adds one root).
fn tls_config(options: &MeteringProxyOptions) -> std::sync::Arc<rustls::ClientConfig> {
    #[cfg(test)]
    if let Some(der) = &options.test_root_der {
        return crate::metering::relay::tls_config_with_root(der);
    }
    let _ = options;
    crate::metering::relay::default_tls_config()
}

impl Default for MeteringProxyOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 0,
            sticky: false,
            identity_headers: true,
            max_concurrent_calls: 512,
            max_buffered_bytes: 512 * 1024 * 1024,
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            leases: crate::domain::runs::lease::LeaseOptions::default(),
            max_held_per_run: hold::DEFAULT_MAX_HELD_PER_RUN,
            #[cfg(test)]
            test_root_der: None,
        }
    }
}

/// Where the proxy listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyInfo {
    /// `http://127.0.0.1:<port>`.
    pub url: String,
    /// The bound port.
    pub port: u16,
}

/// Why the proxy did not start.
#[derive(Debug, thiserror::Error)]
pub enum ProxyStartError {
    /// The port could not be bound (in use, or not permitted).
    #[error("the metering proxy could not listen on 127.0.0.1:{port}: {reason}")]
    Bind { port: u16, reason: String },
    /// The bound address is not loopback (never widened).
    #[error("the metering proxy resolved a non-loopback address; it binds 127.0.0.1 only")]
    NonLoopback,
}

/// The header-read timeout (slowloris).
pub(crate) const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// A running proxy. Dropping it stops it at once (no grace).
pub(crate) struct MeteringProxy {
    info: ProxyInfo,
    shared: Arc<forward::ProxyShared>,
    stop: Option<tokio::sync::watch::Sender<Option<Duration>>>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Story 23-2: the Lease sweeper (aborted before the record pump
    /// closes, so the store writer can close).
    sweeper: Option<tokio::task::JoinHandle<()>>,
}

/// What the proxy needs from its engine.
pub(crate) struct ProxyDeps {
    pub snapshot: Arc<SnapshotPublisher>,
    pub writer: WriterHandle,
    pub inbox: Arc<EvaluationInbox>,
    pub diagnostics: Arc<dyn Fn(String) + Send + Sync>,
    /// Story 22-2: the custody cache — the swap's value source (in-memory
    /// only, refreshed off the hot path).
    pub custody: Arc<crate::domain::custody::CustodyCache>,
}

impl MeteringProxy {
    /// Bind `127.0.0.1:<port>` and start serving on `handle`.
    pub(crate) fn start(
        handle: &tokio::runtime::Handle,
        options: &MeteringProxyOptions,
        deps: ProxyDeps,
    ) -> Result<Self, ProxyStartError> {
        let port = options.port;
        let std_listener = std::net::TcpListener::bind(("127.0.0.1", port)).map_err(|err| {
            ProxyStartError::Bind {
                port,
                reason: err.to_string(),
            }
        })?;
        let addr: SocketAddr = std_listener
            .local_addr()
            .map_err(|err| ProxyStartError::Bind {
                port,
                reason: err.to_string(),
            })?;
        if !addr.ip().is_loopback() {
            return Err(ProxyStartError::NonLoopback);
        }
        std_listener
            .set_nonblocking(true)
            .map_err(|err| ProxyStartError::Bind {
                port,
                reason: err.to_string(),
            })?;
        let listener = {
            let _guard = handle.enter();
            TcpListener::from_std(std_listener).map_err(|err| ProxyStartError::Bind {
                port,
                reason: err.to_string(),
            })?
        };
        let pump = Arc::new(records::RecordPump {
            permits: Arc::new(Semaphore::new(records::RECORD_PERMITS)),
            writer: std::sync::Mutex::new(Some(deps.writer.clone())),
            runs: Arc::clone(&deps.snapshot),
            inbox: deps.inbox,
            stats: Arc::default(),
            diagnostics: Arc::clone(&deps.diagnostics),
            leases: options.leases,
            #[cfg(test)]
            fail_closes: Arc::default(),
            #[cfg(test)]
            prerecord_closes: Arc::default(),
        });
        let kib = (options.max_buffered_bytes / 1024).clamp(1, u64::from(u32::MAX >> 3));
        let shared = Arc::new(forward::ProxyShared {
            snapshot: deps.snapshot,
            pump,
            client: crate::metering::relay::proxy_client(tls_config(options)),
            identity_headers: options.identity_headers,
            inflight: Arc::new(Semaphore::new(options.max_concurrent_calls.max(1))),
            max_calls: options.max_concurrent_calls.max(1),
            buffered_kib: Arc::new(Semaphore::new(kib as usize)),
            counters: forward::RefusalCounters::default(),
            authority: format!("127.0.0.1:{}", addr.port()),
            owner: crate::ports::ProcessBackend::self_fingerprint(&crate::backends::current()),
            stopping: Arc::new(AtomicBool::new(false)),
            pending: std::sync::Mutex::default(),
            diagnostics: deps.diagnostics,
            last_note_minute: std::sync::atomic::AtomicU64::new(0),
            custody: deps.custody,
            allowed_hosts: options.allowed_hosts.clone().into(),
            allowed_origins: options.allowed_origins.clone().into(),
            holds: Arc::new(hold::HoldQueues::new(options.max_held_per_run)),
            #[cfg(test)]
            ensure_delay: std::sync::Mutex::new(None),
        });
        let (stop, stop_rx) = tokio::sync::watch::channel(None);
        let task = handle.spawn(accept_loop(listener, Arc::clone(&shared), stop_rx));
        let sweeper = spawn_sweeper(
            handle,
            deps.writer,
            Arc::clone(&shared.pump.runs),
            Arc::clone(&shared.pump.inbox),
            options.leases,
            Arc::clone(&shared.pump.diagnostics),
            Arc::clone(&shared.holds),
        );
        Ok(Self {
            info: ProxyInfo {
                url: format!("http://127.0.0.1:{}", addr.port()),
                port: addr.port(),
            },
            shared,
            stop: Some(stop),
            task: Some(task),
            sweeper: Some(sweeper),
        })
    }

    /// Where it listens.
    pub(crate) fn info(&self) -> &ProxyInfo {
        &self.info
    }

    /// The shared call state (the seam tests drive its test hooks).
    #[cfg(test)]
    pub(crate) fn shared(&self) -> &Arc<forward::ProxyShared> {
        &self.shared
    }

    /// Stop gracefully: stop accepting, let in-flight calls finish within
    /// `grace`, abort the rest (recorded `shutdown`), then close the record
    /// pump so the store writer can close.
    pub(crate) async fn shutdown(&mut self, grace: Duration) {
        if let Some(sweeper) = self.sweeper.take() {
            sweeper.abort();
            let _ = sweeper.await;
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(Some(grace));
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        self.shared.pump.close();
    }
}

impl Drop for MeteringProxy {
    fn drop(&mut self) {
        self.shared
            .stopping
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(sweeper) = self.sweeper.take() {
            sweeper.abort();
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.shared.pump.close();
    }
}

/// The Lease sweeper (story 23-2): one writer job per tick marks expired
/// or idle Runs `silent` and ends Runs silent past the window, publishing
/// before return; the Run events reach the bus through the evaluation
/// inbox (drained on the reaper tick).
fn spawn_sweeper(
    handle: &tokio::runtime::Handle,
    writer: WriterHandle,
    runs: Arc<SnapshotPublisher>,
    inbox: Arc<EvaluationInbox>,
    leases: crate::domain::runs::lease::LeaseOptions,
    diagnostics: Arc<dyn Fn(String) + Send + Sync>,
    holds: Arc<hold::HoldQueues>,
) -> tokio::task::JoinHandle<()> {
    handle.spawn(async move {
        // Story 23-4: the task also reports each held Run's parked count
        // and oldest start, coalesced to once a second; at start nothing is
        // parked (parked calls are connections, not rows).
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let sweep_every = Duration::from_secs(leases.sweep_interval_secs.max(1));
        let mut last_sweep: Option<std::time::Instant> = None;
        let mut last_note: Option<std::time::Instant> = None;
        let _ = writer
            .call_async(|ctx| ctx.store().reset_held_stats())
            .await;
        loop {
            tick.tick().await;
            let reports = holds.take_reports();
            if !reports.is_empty() {
                let _ = writer
                    .call_async(move |ctx| {
                        for (run_id, count, oldest) in &reports {
                            ctx.store()
                                .set_held_stats(run_id, *count as u64, oldest.as_deref())?;
                        }
                        Ok(())
                    })
                    .await;
            }
            if last_sweep.is_some_and(|at| at.elapsed() < sweep_every) {
                continue;
            }
            last_sweep = Some(std::time::Instant::now());
            let runs = Arc::clone(&runs);
            let inbox = Arc::clone(&inbox);
            let swept = writer
                .call_async(move |ctx| {
                    let changes = crate::domain::runs::lease::sweep(
                        ctx.store(),
                        &leases,
                        crate::time::now_unix_secs(),
                    )?;
                    if changes.events.is_empty() && changes.entries.is_empty() {
                        return Ok(());
                    }
                    let events = crate::domain::runs::ops::commit(ctx, &runs, changes)?;
                    ctx.after_commit(move || inbox.push_run_events(events));
                    Ok(())
                })
                .await;
            if let Err(err) = swept {
                let quiet = last_note.is_some_and(|at| at.elapsed() < Duration::from_secs(60));
                if !quiet && !matches!(err, crate::ports::StoreError::Busy { .. }) {
                    last_note = Some(std::time::Instant::now());
                    diagnostics(format!("the Lease sweeper could not run this pass ({err})"));
                }
            }
        }
    })
}

async fn accept_loop(
    listener: TcpListener,
    shared: Arc<forward::ProxyShared>,
    mut stop: tokio::sync::watch::Receiver<Option<Duration>>,
) {
    let mut connections = tokio::task::JoinSet::new();
    let grace = loop {
        tokio::select! {
            changed = stop.changed() => {
                let grace = if changed.is_ok() { *stop.borrow() } else { None };
                break grace.unwrap_or(Duration::ZERO);
            }
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else { continue };
                let _ = stream.set_nodelay(true);
                let io = TokioIo::new(stream);
                let conn_shared = Arc::clone(&shared);
                connections.spawn(async move {
                    let service = service_fn(move |req| {
                        let shared = Arc::clone(&conn_shared);
                        async move {
                            Ok::<_, std::convert::Infallible>(forward::serve(shared, req).await)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_READ_TIMEOUT)
                        .keep_alive(true)
                        .serve_connection(io, service)
                        .await;
                });
                // Reap finished connections so the set stays small.
                while connections.try_join_next().is_some() {}
            }
        }
    };
    drop(listener);
    // In-flight calls finish within the grace; the rest are aborted (their
    // drop guards record `shutdown`).
    let deadline = tokio::time::Instant::now() + grace;
    while !connections.is_empty() {
        // An idle keep-alive connection has no call in flight: once every
        // admission slot is free, nothing is left to wait for.
        if shared.inflight.available_permits() >= shared.max_calls {
            break;
        }
        if tokio::time::timeout_at(deadline, connections.join_next())
            .await
            .is_err()
        {
            break;
        }
    }
    shared
        .stopping
        .store(true, std::sync::atomic::Ordering::Release);
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}
