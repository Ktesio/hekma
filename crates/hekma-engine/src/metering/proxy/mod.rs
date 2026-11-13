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

pub(crate) mod forward;
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
}

/// What the proxy needs from its engine.
pub(crate) struct ProxyDeps {
    pub snapshot: Arc<SnapshotPublisher>,
    pub writer: WriterHandle,
    pub inbox: Arc<EvaluationInbox>,
    pub diagnostics: Arc<dyn Fn(String) + Send + Sync>,
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
            writer: std::sync::Mutex::new(Some(deps.writer)),
            runs: Arc::clone(&deps.snapshot),
            inbox: deps.inbox,
            stats: Arc::default(),
            diagnostics: Arc::clone(&deps.diagnostics),
            #[cfg(test)]
            fail_closes: Arc::default(),
        });
        let kib = (options.max_buffered_bytes / 1024).clamp(1, u64::from(u32::MAX >> 3));
        let shared = Arc::new(forward::ProxyShared {
            snapshot: deps.snapshot,
            pump,
            client: crate::metering::relay::proxy_client(
                crate::metering::relay::default_tls_config(),
            ),
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
            #[cfg(test)]
            ensure_delay: std::sync::Mutex::new(None),
        });
        let (stop, stop_rx) = tokio::sync::watch::channel(None);
        let task = handle.spawn(accept_loop(listener, Arc::clone(&shared), stop_rx));
        Ok(Self {
            info: ProxyInfo {
                url: format!("http://127.0.0.1:{}", addr.port()),
                port: addr.port(),
            },
            shared,
            stop: Some(stop),
            task: Some(task),
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
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.shared.pump.close();
    }
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
