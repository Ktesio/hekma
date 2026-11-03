//! The Hekma long-running Service Host (story 17-2, the AD-2 amendment of
//! 2026-10-19): owns ONE state directory for a process lifetime — holds the
//! single-writer host lock, opens the [`Engine`] with the Host serve options
//! (fixed `metering.listen` port + the child-environment allowlist), waits
//! for a shutdown signal, and drains every running agent through the
//! engine's bounded stop ladder before letting the engine drop.
//!
//! The crate is a LIB-first, sync-surface host: [`serve`] is a plain blocking
//! call (the `hekma serve` CLI subcommand drives it directly — the CLI stays
//! free of async/tokio tokens, the embed-clean discipline). Internally the
//! host owns one tokio [`Runtime`] — the same shape the engine itself uses —
//! on which the shutdown-signal wait and the engine's async drain calls run.
//! The optional Control API (story 17-3, `--listen` + `--bearer-secret`)
//! mounts on that SAME runtime after the engine opens; without the flags it
//! does not exist and serve is byte-identical to 17-2.
//!
//! Dependency posture (NFR-8 / AD-2): the ONLY normal dependency is
//! [`hekma_engine`](hekma_engine)'s PUBLIC API. The engine stays the sole
//! path authority (the state root resolves through the public
//! `EnginePaths::new` seam; the lock file name is
//! `hekma_engine::paths::HOST_LOCK_FILE`) and the sole process-control
//! authority (stops go through `Engine::stop`'s bounded ladder — the host
//! never signals a process itself, AD-20).
//!
//! Notice discipline (AI-18 surfaced-not-silent): every host event of
//! operator consequence — startup + state root, a lock refusal, the signal
//! receipt, each per-instance stop outcome, shutdown completion — is emitted
//! as ONE `[hekma] `-prefixed line through the same [`DiagnosticSink`]
//! contract the engine's own diagnostics use (a caller-provided sink, or
//! stderr by default). A refusal additionally returns its typed
//! [`ServeError`]; with no caller sink the error text surfaces exactly once
//! (the caller prints it), with a sink it is noted AND returned.

#![forbid(unsafe_code)]

mod control;
mod drain;
mod local;
mod lock;
mod otlp;
mod signals;
pub mod wire;

/// The response header that carries a write request's engine notes, and its
/// codec (story 21-4): the `hekma` CLI writes the decoded bytes to its stderr.
pub mod engine_notes {
    pub use crate::control::notes::{decode, encode, ENGINE_NOTES_HEADER};
}

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub use drain::{drain_and_stop, drain_plan, DrainOutcome, StopResult};
pub use local::{
    control_file, read_discovery, serve_log_file, spawn_detached, write_discovery, Discovery,
    CONTROL_FILE, DISCOVERY_SCHEMA_VERSION, SERVE_LOG_FILE,
};
pub use lock::{read_holder_pid, HostLock, LockRefusal};
// The exporter's operator-facing configuration types (AD-21): ServeOptions
// carries the parsed flags; the parse itself lives on ONE site
// (`OtlpOptions::from_flags`) both CLI surfaces call.
pub use otlp::{OtlpExporter, OtlpHeaderValue, OtlpOptions};

/// The engine's diagnostic sink contract, re-used verbatim: one
/// `Arc<Mutex<Box<dyn Write + Send>>>` the host's (and the engine's)
/// `[hekma] `-prefixed notices write through.
pub use hekma_engine::DiagnosticSink;

/// Options for one [`serve`] lifetime. `Default` is the CLI's defaults: the
/// platform state root (the `HEKMA_STATE_DIR`/`KTESIO_STATE_DIR` env override
/// honored by the engine's path authority), a 30-second stop window (the
/// engine's `DEFAULT_STOP_WINDOW`, never re-hardcoded here), an OS-ephemeral
/// metering port, and stderr notices.
#[derive(Clone, Default)]
pub struct ServeOptions {
    /// The state-dir base override (`Some` = verbatim, `None` = the engine's
    /// resolution order). The SAME base the engine will open — the host lock
    /// and the engine must always agree on the root, which is why the host
    /// resolves it through the engine's own `EnginePaths` seam.
    pub state_base: Option<PathBuf>,
    /// The graceful stop window in SECONDS for the shutdown drain
    /// (`None` = the engine's `DEFAULT_STOP_WINDOW`). Every running agent
    /// gets this window to exit on SIGTERM before the ladder escalates.
    /// Validated against [`MIN_STOP_WINDOW_SECS`]..=[`MAX_STOP_WINDOW_SECS`]
    /// (review finding 9); an out-of-bounds value is
    /// [`ServeError::InvalidOptions`].
    pub stop_window: Option<u64>,
    /// The fixed loopback PORT every `engine-observed` instance's metering
    /// listener binds for this host's lifetime (`0` = OS-ephemeral, the
    /// pre-17-2 default). Non-zero is what keeps an adopted observed
    /// instance's injected `base_url` live across host restarts (the AI-46
    /// strand closure).
    pub metering_listen_port: u16,
    /// The address the v0 Control API binds (`None` = API off — the default;
    /// serve is byte-identical to 17-2). Requires [`ServeOptions::bearer_secret`]
    /// (a missing partner is [`ServeError::InvalidOptions`] — the API never
    /// serves unauthenticated and never half-configured).
    pub control_listen: Option<std::net::SocketAddr>,
    /// The NAME of the bearer secret guarding the Control API — resolved ONCE
    /// at startup through the engine's composite resolver (env, then the
    /// state-dir secrets file). Requires [`ServeOptions::control_listen`].
    /// The resolved VALUE never enters options, a response, or a notice.
    pub bearer_secret: Option<String>,
    /// The engine's log-capture posture for this host's lifetime (story 17-4,
    /// the `--log-capture` thread): `full` (the default) is byte-identical to
    /// 17-3; `metadata` keeps agent content off persistent disk at rest;
    /// `off` captures nothing. Open-time only — every spawn this host
    /// supervises resolves the SAME mode.
    pub log_capture: hekma_engine::LogCaptureMode,
    /// The sink for host + engine notices. `None` (the default) routes both
    /// to stderr, byte-compatible with the engine's no-sink behavior.
    pub diagnostics: Option<DiagnosticSink>,
    /// The OTLP telemetry exporter's configuration (AD-21). The DEFAULT is
    /// the exporter ABSENT — with no endpoint configured no task is
    /// spawned and nothing dials out (the structural privacy default,
    /// test-proven). Header values may be `secret:NAME` references; their
    /// Debug (and this struct's) never shows one.
    pub otlp: OtlpOptions,
    /// Story 21-4: the PEM certificate chain for the `--listen` listener's
    /// TLS. Goes with [`ServeOptions::tls_key`] (one alone is
    /// [`ServeError::InvalidOptions`]); required for a NON-loopback
    /// `control_listen`. The local endpoint never uses TLS.
    pub tls_cert: Option<PathBuf>,
    /// Story 21-4: the PEM private key for [`ServeOptions::tls_cert`].
    pub tls_key: Option<PathBuf>,
    /// Story 21-4: AUTO mode — the host a `hekma` CLI command starts in the
    /// background when no host serves the state root. It refuses the
    /// explicit-listener flags (`control_listen`, `bearer_secret`, TLS),
    /// ignores SIGHUP, passes the starting command's environment to agents
    /// (no child-environment allowlist), and exits through the normal drain
    /// when no agent is live and no request arrived for
    /// [`AUTOSTART_IDLE_WINDOW`], or when its state root disappears.
    pub autostart: bool,
    /// Story 21-4: the build version `control.json` records (the CLI notes a
    /// host from another build). `None` records this crate's version.
    pub build_version: Option<String>,
    /// Story 21-4's TEST SCALING SEAM for the auto mode's idle window:
    /// `Some(window)` overrides [`AUTOSTART_IDLE_WINDOW`]. `None` (every
    /// production caller) keeps the 30 s window. Not an operator surface.
    #[doc(hidden)]
    pub autostart_idle_override: Option<std::time::Duration>,
}

/// Story 21-4: how long an AUTO-STARTED host waits with no live agent and no
/// request before it drains and exits (the tmux-server pattern).
pub const AUTOSTART_IDLE_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);

/// How often the auto-started host checks for idleness and for its state
/// root.
const AUTOSTART_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// The inclusive lower bound of every stop-window option (whole seconds):
/// `0` would make the graceful ladder an instant kill — refused everywhere
/// (clap range on the CLI surfaces, [`ServeError::InvalidOptions`] here).
pub const MIN_STOP_WINDOW_SECS: u64 = 1;

/// The inclusive upper bound of every stop-window option (whole seconds, =
/// one day): a bounded stop is a service-drain budget, not a lifetime —
/// refused everywhere, same as the lower bound.
pub const MAX_STOP_WINDOW_SECS: u64 = 86_400;

impl std::fmt::Debug for ServeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Manual impl: a boxed writer sink is not `Debug` and must never be
        // printed (no-leak discipline). The bearer field is the secret NAME
        // (never a value — resolution happens inside `serve`), printed as
        // the configuration fact it is; the RESOLVED value never enters this
        // struct at all.
        f.debug_struct("ServeOptions")
            .field("state_base", &self.state_base)
            .field("stop_window", &self.stop_window)
            .field("metering_listen_port", &self.metering_listen_port)
            .field("control_listen", &self.control_listen)
            .field("bearer_secret", &self.bearer_secret)
            .field("log_capture", &self.log_capture)
            .field("diagnostics", &self.diagnostics.as_ref().map(|_| "<sink>"))
            // The OtlpOptions Debug redacts header VALUES (the no-leak
            // discipline: a literal header value IS secret material).
            .field("otlp", &self.otlp)
            .field("tls_cert", &self.tls_cert)
            .field("tls_key", &self.tls_key)
            .field("autostart", &self.autostart)
            .field("build_version", &self.build_version)
            .field("autostart_idle_override", &self.autostart_idle_override)
            .finish()
    }
}

/// Why [`serve`] refused to run or could not start. The CLI maps the
/// variants onto the house exit-code table (review finding 8):
/// `StateDirLocked` and `InvalidOptions` are Usage-class (exit `2` — the
/// invocation cannot proceed / the flag value is invalid), while `Startup`
/// is the IO/system class (exit `1` — state-dir creation, lock I/O, engine
/// open, runtime build, signal registration).
#[derive(Debug)]
pub enum ServeError {
    /// Another host holds the state directory's `host.lock`. `holder` is the
    /// live holder's PID when the lock file's `pid=<n>` record could be read
    /// (`None` = held, but the record is unreadable — the lock is real
    /// either way). Carries the lock path so the remediation is actionable.
    StateDirLocked {
        /// The `<state-root>/host.lock` path that is held.
        lock_path: PathBuf,
        /// The holder's PID, when readable.
        holder: Option<u32>,
    },
    /// The caller's options are invalid — a stop window outside
    /// [`MIN_STOP_WINDOW_SECS`]..=[`MAX_STOP_WINDOW_SECS`] (review finding
    /// 9: a sub-second or multi-day window is a rejected INVOCATION, not a
    /// failed environment), or a HALF-configured Control API (one of
    /// `control_listen` / `bearer_secret` without its partner — the API
    /// never serves unauthenticated, never half-configured; story 17-3).
    /// Usage-class, like the CLI's parse refusal.
    InvalidOptions {
        /// The human-readable rejection (names the value and the bounds).
        message: String,
    },
    /// The host could not start (state-dir resolution/creation, lock I/O,
    /// engine open, runtime build, signal-stream registration). The message
    /// is the surfaced reason — one line, no secret material (path + OS
    /// error text only). IO/system-class per the house exit table (review
    /// finding 8): this is the ENVIRONMENT failing, not the invocation.
    Startup {
        /// The human-readable failure reason.
        message: String,
    },
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Review finding 10: the remediation names the LIVE holder and
            // the two real options — stop that host, or serve a different
            // state directory. NO file-removal advice: a dead holder
            // releases the OS lock by itself (the stale pid record is
            // re-acquired across, never an obstacle), and telling an
            // operator to delete the lock file invites deleting a LIVE
            // holder's lock — a third serve would then create a fresh
            // `host.lock` inode and TWO hosts would serve one state root.
            ServeError::StateDirLocked { lock_path, holder } => match holder {
                Some(pid) => write!(
                    f,
                    "the state directory is already served by another host (pid {pid}, \
                     lock {lock_path}) — stop that host first, or serve a different \
                     state directory",
                    lock_path = lock_path.display(),
                ),
                None => write!(
                    f,
                    "the state directory is already served by another host (lock \
                     {lock_path}; its pid record could not be read) — stop that host \
                     first, or serve a different state directory",
                    lock_path = lock_path.display(),
                ),
            },
            ServeError::InvalidOptions { message } => write!(f, "{message}"),
            ServeError::Startup { message } => {
                write!(f, "the host could not start: {message}")
            }
        }
    }
}

impl std::error::Error for ServeError {}

/// The host's notice emitter: the ONE choke point every host notice goes
/// through, over the engine's [`DiagnosticSink`] contract. Same prefix, same
/// one-line-`\n`-terminated shape, best-effort writes (a broken sink never
/// kills the host — the engine's emission contract, mirrored).
#[derive(Clone)]
pub(crate) struct Notifier {
    sink: DiagnosticSink,
}

impl Notifier {
    /// A notifier over the caller-provided sink.
    pub fn over(sink: DiagnosticSink) -> Self {
        Notifier { sink }
    }

    /// The default notifier: stderr, byte-compatible with the engine's
    /// no-sink diagnostic behavior.
    pub fn stderr() -> Self {
        Notifier::over(Arc::new(Mutex::new(Box::new(std::io::stderr()))))
    }

    /// Emit ONE `[hekma] `-prefixed notice line (best-effort write + flush).
    ///
    /// Review finding 17: a POISONED sink mutex is recovered
    /// (`PoisonError::into_inner` — the engine's own emission discipline):
    /// a sink whose writer panicked mid-`write` must never silence the
    /// operator stream for the rest of the host's life. The write itself
    /// stays best-effort (a broken sink never kills the host).
    pub fn note(&self, text: &str) {
        let line = format!("[hekma] {text}\n");
        let mut writer = match self.sink.lock() {
            Ok(writer) => writer,
            // Poisoned by a panicking writer: take the guard anyway. The
            // data is the stream; a torn line is better than NO line.
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = std::io::Write::write_all(&mut *writer, line.as_bytes());
            let _ = std::io::Write::flush(&mut *writer);
        }));
    }
}

/// Serve one state directory until a shutdown signal arrives (the long-running
/// host's whole contract, in order):
///
/// 1. Resolve the state root through the engine's public `EnginePaths` seam
///    and create it (the host lock runs BEFORE `Engine::open`, so the
///    directory must exist first).
/// 2. Acquire the single-writer `host.lock` (a second host on the same root
///    is refused: typed [`ServeError::StateDirLocked`] naming the holder PID
///    + remediation, nothing opened).
/// 3. Build the host runtime and ARM the shutdown-signal streams — both
///    BEFORE the engine opens (review finding 1): a SIGTERM arriving during
///    the open (adoption can take real time) is captured by the live
///    streams and held, never delivered to the default disposition, and the
///    drain still runs once the open settles.
/// 4. Open the [`Engine`](hekma_engine::Engine) with the Host serve options:
///    the fixed `metering.listen` port (when set) and the child-environment
///    allowlist (ALWAYS on under serve — hosted agents inherit ONLY their
///    declared env plus the documented OS-baseline floor, never the host
///    shell's ambient variables).
/// 5. Wait for the OS shutdown signal (SIGTERM, or SIGINT as the foreground
///    equivalent; Ctrl+C on Windows) — including one that arrived during
///    step 4's open. Registration failure at any point is a typed
///    [`ServeError::Startup`], never a panic (review finding 2); repeats
///    during the drain are swallowed and noted, never lethal (finding 3).
/// 6. Drain: stop every RUNNING agent through `Engine::stop`'s bounded
///    ladder with the configured window — CONCURRENTLY (one window's wall
///    clock, finding 4) — re-enumerating once for newcomers and surfacing
///    any straggler honestly (finding 5). The engine is NOT dropped until
///    every stop has settled.
/// 7. Emit the shutdown-complete notice (naming the received signal) and
///    return.
///
/// The Control API (story 17-3; story 21-4 made it every serve's): right
/// after the started notice the API binds its LOOPBACK LOCAL ENDPOINT
/// (127.0.0.1, an OS-chosen port, a fresh 256-bit operator token) and —
/// with `control_listen` + `bearer_secret` — the operator's listener too
/// (TLS with `tls_cert` + `tls_key`; a non-loopback address without them is
/// [`ServeError::InvalidOptions`] before anything binds or opens). Then
/// `control.json` records the local endpoint for the CLI. The bearer secret
/// resolves through the engine's composite AFTER the lock and BEFORE the
/// runtime (an unresolvable reference is a typed [`ServeError::Startup`] —
/// nothing binds, nothing opens); a bind failure is a surfaced `Startup`
/// naming the address. On shutdown the API stops accepting FIRST —
/// graceful, a bounded grace, then the SSE streams abort — `control.json`
/// is removed, and only then does step 6 run. In AUTO mode
/// ([`ServeOptions::autostart`]) step 5 also ends when the host is idle or
/// its state root disappears.
///
/// Errors: [`ServeError::StateDirLocked`] / [`ServeError::InvalidOptions`] /
/// [`ServeError::Startup`] — the first two BEFORE any supervision work, so
/// a refused host leaves no partial state.
pub fn serve(options: ServeOptions) -> Result<(), ServeError> {
    serve_with_signal(options, signals::wait_for_signal)
}

/// [`serve`] with the shutdown-signal SOURCE supplied by the caller — the
/// seam tests (and alternative hosts) drive. The factory receives the
/// host's diagnostic sink (the real source notes swallowed repeat signals
/// through it) and is invoked INSIDE the host's runtime, during the arming
/// phase — before the engine opens. Its future resolving (to the signal
/// NAME, or a registration/verification error string) triggers the drain
/// exactly as an OS signal would. [`serve`] passes the real OS-signal
/// waiter.
pub fn serve_with_signal<F>(
    options: ServeOptions,
    signal: impl FnOnce(DiagnosticSink) -> F,
) -> Result<(), ServeError>
where
    F: std::future::Future<Output = Result<&'static str, String>> + Send + 'static,
{
    let ServeOptions {
        state_base,
        stop_window,
        metering_listen_port,
        control_listen,
        bearer_secret,
        log_capture,
        diagnostics,
        otlp,
        tls_cert,
        tls_key,
        autostart,
        build_version,
        autostart_idle_override,
    } = options;
    let has_caller_sink = diagnostics.is_some();
    let notifier = match diagnostics {
        Some(sink) => Notifier::over(sink.clone()),
        None => Notifier::stderr(),
    };
    // Every pre-start refusal: noted through a caller sink (AI-18) AND
    // returned; with no caller sink the caller's own print is the single
    // surfacing.
    let refuse = |err: ServeError| -> ServeError {
        if has_caller_sink {
            notifier.note(&err.to_string());
        }
        err
    };

    // (0) The options, validated BEFORE anything is touched (review finding
    // 9's discipline, extended to the Control-API pair): an out-of-bounds
    // window, a malformed listen address, or a HALF-configured API (one flag
    // without its partner — the API never serves unauthenticated, never
    // half-configured) is a rejected INVOCATION (`InvalidOptions`), never a
    // runtime surprise. `None` keeps the engine's DEFAULT_STOP_WINDOW (the
    // single source of the default — never re-hardcoded here).
    let window = std::time::Duration::from_secs(match stop_window {
        None => hekma_engine::domain::DEFAULT_STOP_WINDOW.as_secs(),
        Some(secs) if (MIN_STOP_WINDOW_SECS..=MAX_STOP_WINDOW_SECS).contains(&secs) => secs,
        Some(secs) => {
            return Err(refuse(ServeError::InvalidOptions {
                message: format!(
                    "invalid stop window {secs}: the graceful window must be a whole \
                     number of seconds from {MIN_STOP_WINDOW_SECS} to \
                     {MAX_STOP_WINDOW_SECS}"
                ),
            }));
        }
    });
    if autostart
        && (control_listen.is_some()
            || bearer_secret.is_some()
            || tls_cert.is_some()
            || tls_key.is_some())
    {
        return Err(refuse(ServeError::InvalidOptions {
            message: "--autostart refuses the explicit-listener flags (--listen, \
                      --bearer-secret, --tls-cert, --tls-key): an auto-started host serves \
                      only its loopback local endpoint"
                .to_string(),
        }));
    }
    match (&control_listen, &bearer_secret) {
        (None, None) => {}
        (Some(_), Some(_)) => {}
        (Some(addr), None) => {
            return Err(refuse(ServeError::InvalidOptions {
                message: format!(
                    "--listen {addr} requires --bearer-secret: the Control API never \
                     serves unauthenticated; pass both flags (or neither, to keep the \
                     API off)"
                ),
            }));
        }
        (None, Some(_)) => {
            return Err(refuse(ServeError::InvalidOptions {
                message: "--bearer-secret requires --listen: there is no API to guard \
                          while the Control API is off; pass both flags (or neither, to \
                          keep the API off)"
                    .to_string(),
            }));
        }
    }
    // (0.2) TLS (story 21-4, AD-20 amended 2026-10-30): the pair goes
    // together, applies to the explicit listener only, and is REQUIRED
    // beyond loopback. The PEM files are read and the server config built
    // HERE — before the lock — so an unreadable file or a bad key is a typed
    // usage refusal naming the path, and nothing binds or opens.
    let tls_config = match (&tls_cert, &tls_key) {
        (None, None) => None,
        (Some(_), None) | (None, Some(_)) => {
            return Err(refuse(ServeError::InvalidOptions {
                message: "--tls-cert and --tls-key go together: pass both PEM files (or \
                          neither, for a loopback plain-HTTP listener)"
                    .to_string(),
            }));
        }
        (Some(cert), Some(key)) => {
            if control_listen.is_none() {
                return Err(refuse(ServeError::InvalidOptions {
                    message: "--tls-cert/--tls-key require --listen: TLS applies to the \
                              operator's listener (the local endpoint is loopback plain \
                              HTTP)"
                        .to_string(),
                }));
            }
            match control::tls::load_server_config(cert, key) {
                Ok(config) => Some(config),
                Err(message) => return Err(refuse(ServeError::InvalidOptions { message })),
            }
        }
    };
    if let Some(addr) = control_listen {
        if !addr.ip().is_loopback() && tls_config.is_none() {
            return Err(refuse(ServeError::InvalidOptions {
                message: format!(
                    "--listen {addr} is not a loopback address: beyond loopback the Control \
                     API requires TLS — pass --tls-cert <pem> and --tls-key <pem> (or bind \
                     127.0.0.1)"
                ),
            }));
        }
    }

    // (0.5) The OTLP exporter's options, validated BEFORE anything is
    // touched (the same rejected-INVOCATION class): a non-http endpoint,
    // an out-of-bounds flush interval, headers without an endpoint, or a
    // malformed header spec is `InvalidOptions` — never a runtime surprise
    // mid-open. The default (no endpoint) validates trivially: the
    // exporter simply does not exist.
    let otlp_options = otlp;
    if let Err(message) = otlp_options.validate() {
        return Err(refuse(ServeError::InvalidOptions { message }));
    }

    // (1) The state root, via the engine's path authority (the host NEVER
    // joins path segments of the state layout itself — only the engine's own
    // HOST_LOCK_FILE const names the lock file).
    let paths = hekma_engine::paths::EnginePaths::new(state_base.clone()).map_err(|err| {
        ServeError::Startup {
            message: err.to_string(),
        }
    })?;
    let state_root = paths.state_base().to_path_buf();
    let lock_path = state_root.join(hekma_engine::paths::HOST_LOCK_FILE);
    // The host lock runs BEFORE Engine::open, and Engine::open is what would
    // otherwise create the directory — create it here, idempotently.
    std::fs::create_dir_all(&state_root).map_err(|err| ServeError::Startup {
        message: format!(
            "could not create the state directory {}: {err}",
            state_root.display()
        ),
    })?;

    // (2) The single-writer lock. With a caller sink the refusal is noted
    // through it AND returned (AI-18); with none, the caller's own print of
    // the returned error is the single surfacing (no double line).
    let _lock = match HostLock::acquire(&lock_path) {
        Ok(lock) => lock,
        Err(LockRefusal::Held { holder }) => {
            return Err(refuse(ServeError::StateDirLocked {
                lock_path: lock_path.clone(),
                holder,
            }));
        }
        Err(LockRefusal::Io(err)) => {
            return Err(refuse(ServeError::Startup {
                message: format!(
                    "could not acquire the host lock {}: {err}",
                    lock_path.display()
                ),
            }));
        }
    };

    // (2.5) The Control API's bearer secret — resolved ONCE, through the
    // engine's composite (env first, then the state-dir secrets file — the
    // host never builds a second resolution mechanism, AD-10), AFTER the
    // lock and BEFORE the runtime/engine open: an unresolvable reference is
    // a typed `Startup` refusal and NOTHING binds, NOTHING opens. A
    // half-configured pair was already refused above.
    let bearer = match &bearer_secret {
        None => None,
        Some(name) => {
            let resolver =
                hekma_engine::ports::CompositeSecretResolver::env_then_file(paths.secrets_file());
            match resolver.require(name) {
                Ok(secret) => {
                    // An EMPTY resolved secret can never authenticate (an
                    // empty presented bearer is refused by construction), so
                    // binding would mint an API that 401s every request
                    // forever with no diagnosis. Refuse NOW, typed, naming
                    // the reference (review item 2).
                    if secret.expose_secret().is_empty() {
                        return Err(refuse(ServeError::Startup {
                            message: format!(
                                "the control API's bearer secret {name:?} resolved to an \
                                 EMPTY value — an empty bearer can never authenticate; set \
                                 a non-empty value for it (env or the secrets file)"
                            ),
                        }));
                    }
                    Some(secret)
                }
                Err(err) => {
                    return Err(refuse(ServeError::Startup {
                        message: format!(
                            "the control API's bearer secret could not be resolved: {err}"
                        ),
                    }));
                }
            }
        }
    };

    // (2.6) The OTLP exporter's secret header references — resolved ONCE,
    // through the same composite the bearer uses (env, then the state-dir
    // secrets file — AD-10), AFTER the lock and BEFORE the runtime: an
    // unresolvable reference is a typed refusal NAMING the secret and
    // NOTHING binds, NOTHING opens. The RESOLVED values are consumed only by
    // the HTTP client — never options, notices, or logs.
    let otlp_headers_resolved = if otlp_options.configured() {
        let resolver =
            hekma_engine::ports::CompositeSecretResolver::env_then_file(paths.secrets_file());
        match otlp_options.resolve_headers(&resolver) {
            Ok(headers) => Some(headers),
            Err(message) => return Err(refuse(ServeError::InvalidOptions { message })),
        }
    } else {
        None
    };

    // (2.7) The local operator token (story 21-4): 256 bits from the OS
    // CSPRNG, minted per serve. Its plaintext lives only in `control.json`
    // and in the CLI's request; the resolver keeps its SHA-256.
    let local_token =
        local::mint_token().map_err(|message| refuse(ServeError::Startup { message }))?;
    let resolver = {
        let mut credentials = vec![(
            local_token.as_str(),
            control::CredentialClass::OperatorBearer,
        )];
        if let Some(secret) = &bearer {
            credentials.push((
                secret.expose_secret(),
                control::CredentialClass::OperatorBearer,
            ));
        }
        control::CredentialResolver::new(credentials)
    };
    drop(bearer);

    // (3) The host runtime — built BEFORE the engine opens (review finding
    // 1): a runtime-build failure must not kill an engine that already
    // adopted agents.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|err| ServeError::Startup {
            message: format!("could not start the host runtime: {err}"),
        })?;

    // (3.5) AUTO mode ignores SIGHUP (story 21-4): closing the terminal whose
    // command started this host must not end it.
    if autostart {
        rt.block_on(signals::ignore_hangup())
            .map_err(|message| ServeError::Startup { message })?;
    }

    // (4) ARM the shutdown signals BEFORE the engine opens (review finding
    // 1): the factory runs INSIDE the runtime NOW, so the real OS streams
    // register before any adoption work — a signal landing during the open
    // is captured and held, never defaulted. The handshake loop gives the
    // spawned task its first polls (which is when the real source registers
    // its streams); a factory that resolved inside the arming window — a
    // registration FAILURE (finding 2: typed Startup, before any engine
    // work) or an implausibly early signal — is joined and branched on
    // right here. The bounded hop count keeps a pathological scheduler from
    // hanging the host; even at zero completed hops the wait task is live
    // on the runtime, so the open below never runs without an armed wait.
    let sink_for_signals = notifier.sink.clone();
    let (mut wait_task, early_signal, arming_failure) = rt.block_on(async {
        let mut task = tokio::spawn(signal(sink_for_signals));
        let mut early: Option<&'static str> = None;
        let mut failure: Option<ServeError> = None;
        for _ in 0..64 {
            tokio::task::yield_now().await;
            if task.is_finished() {
                match (&mut task).await {
                    Ok(Ok(name)) => early = Some(name),
                    Ok(Err(registration)) => {
                        failure = Some(ServeError::Startup {
                            message: registration,
                        });
                    }
                    Err(join) => {
                        failure = Some(ServeError::Startup {
                            message: format!("the shutdown-signal wait task failed: {join}"),
                        });
                    }
                }
                break;
            }
        }
        (task, early, failure)
    });
    if let Some(err) = arming_failure {
        return Err(err);
    }

    // (5) The engine, with the Host serve options from the FIRST moment —
    // before adoption, so an adopted observed instance re-binds the fixed
    // port on this very open. The child-env allowlist is NOT a flag: under
    // an explicit serve it is always on (a hosted agent inherits ONLY its
    // declared env plus the documented OS-baseline floor). An AUTO-STARTED
    // host (story 21-4) passes the starting command's environment through,
    // exactly as the CLI's own engine did — `export KEY=…; hekma agent start
    // x` keeps working in that shell. `Arc` because the drain's concurrent
    // stop tasks share it (review finding 4). Opened OUTSIDE any `block_on` —
    // it builds its own internal runtime, exactly as before. The engine's
    // diagnostics go through the note tap (story 21-4): every line still
    // reaches the host's stream, and a write request's lines also ride back
    // to its client.
    let note_tap = if autostart {
        control::NoteTap::keeping_startup_notes()
    } else {
        control::NoteTap::default()
    };
    let engine = Arc::new(
        hekma_engine::Engine::open_with_options(
            state_base.clone(),
            hekma_engine::EngineOptions {
                metering_listen_port,
                child_env_allowlist: !autostart,
                // Story 17-4: the engine's log-capture posture — every spawn
                // this host supervises resolves the SAME mode (open-time
                // only, the metering-port thread).
                log_capture,
                // Story 18-3: the host never overrides the engine's pinned
                // stable-run window (the test-only seam stays `None`).
                stable_run_window_override: None,
                // Story 18-4: the host never overrides the engine's pinned
                // standalone pause-command cap (the test-only seam stays
                // `None`).
                pause_command_cap_override: None,
                // Route the ENGINE's own diagnostics (adoption notes, the
                // re-bind notes, enforcement breadcrumbs) through the SAME
                // sink the host notices use — one interleaved, honest stream.
                diagnostics: Some(note_tap.sink_over(notifier.sink.clone())),
            },
        )
        .map_err(|err| ServeError::Startup {
            message: err.to_string(),
        })?,
    );
    notifier.note(&format!(
        "host started; state root {} — supervising agents until SIGTERM/SIGINT",
        state_root.display()
    ));

    // (5.5) The Control API mounts here: AFTER the engine opens and the
    // started notice lands, BEFORE the signal wait. The LOCAL endpoint
    // (story 21-4) always binds; the operator's `--listen` joins it when
    // configured — one router, one resolver. A bind failure (or a failed
    // `control.json` write) is a surfaced `Startup` error and the host
    // ABORTS — after the SAME bounded drain the signal path runs, because
    // the engine has OPENED and may have ADOPTED agents.
    let local_endpoint = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    let mut specs = vec![control::ListenerSpec::Plain(local_endpoint)];
    if let Some(addr) = control_addr(control_listen, &tls_config) {
        specs.push(addr);
    }
    // Bind every socket FIRST, record the local endpoint in `control.json`,
    // and only THEN start serving and print the listening notices: a client
    // that gets an answer, or reads a notice, always finds the discovery
    // file on disk (a connection that arrives in between waits in the accept
    // queue).
    let bound = match rt.block_on(control::BoundListeners::bind(specs)) {
        Ok(bound) => bound,
        Err(message) => {
            let _outcomes = rt.block_on(drain::drain_and_stop(&engine, window, &notifier.sink));
            return Err(refuse(ServeError::Startup { message }));
        }
    };
    let addrs = bound.local_addrs();
    let local_addr = addrs[0];
    let discovery = local::Discovery {
        schema_version: local::DISCOVERY_SCHEMA_VERSION,
        pid: std::process::id(),
        addr: local_addr,
        token: local_token,
        hekma_version: build_version.unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
        autostarted: autostart,
    };
    if let Err(err) = local::write_discovery(&state_root, &discovery) {
        drop(bound);
        let _outcomes = rt.block_on(drain::drain_and_stop(&engine, window, &notifier.sink));
        return Err(refuse(ServeError::Startup {
            message: format!(
                "could not write the discovery file {}: {err}",
                local::control_file(&state_root).display()
            ),
        }));
    }
    drop(discovery);
    let api = match rt.block_on(bound.serve(
        Arc::clone(&engine),
        resolver,
        note_tap,
        notifier.sink.clone(),
    )) {
        Ok(api) => api,
        Err(message) => {
            local::remove_discovery(&state_root, std::process::id());
            let _outcomes = rt.block_on(drain::drain_and_stop(&engine, window, &notifier.sink));
            return Err(refuse(ServeError::Startup { message }));
        }
    };
    notifier.note(&format!(
        "local control endpoint on {local_addr} (loopback, for the hekma CLI; recorded in {})",
        local::control_file(&state_root).display()
    ));
    if let Some(addr) = addrs.get(1) {
        let scheme = if tls_config.is_some() {
            "https"
        } else {
            "http"
        };
        notifier.note(&format!(
            "control api listening on {addr} (v0, unstable — bearer required; {scheme})"
        ));
    }

    // (5.6) The OTLP telemetry exporter mounts here (AD-21) — the SAME
    // mount shape as the Control API: AFTER the engine opens, BEFORE the
    // signal wait. With NO endpoint configured (the default) this block is
    // skipped ENTIRELY — no task is spawned, no socket is dialed, serve is
    // byte-identical to the pre-exporter host (the structural privacy
    // default, asserted by tests/otlp_export.rs). The exporter subscribes
    // READ-ONLY to the engine's public event bus (lock-free, publish-after-
    // commit honored by the bus contract) and degrades visibly — every
    // drop/delivery failure is a surfaced note plus the
    // `hekma.otlp.dropped` self-metric, never an error into supervision.
    let exporter = match otlp_options.configured() {
        false => None,
        true => {
            let headers = otlp_headers_resolved.unwrap_or_default();
            let config = otlp::task::ExporterConfig {
                endpoint: otlp_options
                    .endpoint
                    .clone()
                    .expect("validated configured() means an endpoint"),
                headers,
                flush_interval: std::time::Duration::from_secs(
                    otlp_options.effective_flush_interval(),
                ),
                service_instance_id: otlp::service_instance_id(&state_root),
            };
            let rx = engine.subscribe();
            let exporter =
                rt.block_on(async { otlp::OtlpExporter::start(rx, config, notifier.sink.clone()) });
            Some(exporter)
        }
    };

    // (6) Resolve why the host ends — the received signal (it may already
    // have arrived during the arming window or the open, held by the armed
    // streams), or, in AUTO mode, idleness / a vanished state root. The
    // streams stay armed through the whole drain (the holder task inside the
    // signal source; review finding 3): a second SIGTERM mid-drain is
    // swallowed and noted, never lethal.
    let ending: Ending = match early_signal {
        Some(name) => Ending::Signal(name),
        None => {
            let signal_wait = async {
                match (&mut wait_task).await {
                    Ok(Ok(name)) => Ok(Ending::Signal(name)),
                    Ok(Err(registration)) => Err(ServeError::Startup {
                        message: registration,
                    }),
                    Err(join) => Err(ServeError::Startup {
                        message: format!("the shutdown-signal wait task failed: {join}"),
                    }),
                }
            };
            let outcome = if autostart {
                let idle = autostart_idle_exit(
                    Arc::clone(&engine),
                    api.activity(),
                    state_root.clone(),
                    autostart_idle_override.unwrap_or(AUTOSTART_IDLE_WINDOW),
                );
                rt.block_on(async {
                    tokio::select! {
                        signaled = signal_wait => signaled,
                        reason = idle => Ok(Ending::Auto(reason)),
                    }
                })
            } else {
                rt.block_on(signal_wait)
            };
            match outcome {
                Ok(ending) => ending,
                Err(err) => {
                    let _ = rt.block_on(api.shutdown());
                    local::remove_discovery(&state_root, std::process::id());
                    return Err(err);
                }
            }
        }
    };
    match ending {
        Ending::Signal(name) => notifier.note(&format!(
            "{name} received; stopping running and paused agents (graceful window {}s) ...",
            window.as_secs_f32()
        )),
        Ending::Auto(reason) => notifier.note(&format!(
            "auto-started host exiting ({reason}); stopping running and paused agents \
             (graceful window {}s) ...",
            window.as_secs_f32()
        )),
    }

    // (6.4) The OTLP exporter stops BEFORE/WITH the Control API (the AD-21
    // shutdown order): fire the handshake, await the bounded final flush
    // within the exporter's OWN bound (P7: the task's FINAL_FLUSH_BOUND —
    // the constant, never a re-hardcoded magic number) — the host's exit is
    // never held past it. The task's own notes carry the honest in-flight
    // outcome (lost points named; cumulative sums reset at restart).
    if let Some(exporter) = exporter {
        let lost = rt.block_on(exporter.shutdown(otlp::task::FINAL_FLUSH_BOUND));
        notifier.note("otlp export stopped; telemetry export ends before the shutdown drain");
        match lost {
            Some(0) => {}
            Some(n) => notifier.note(&format!(
                "otlp export: {n} in-flight observation(s) were NOT delivered at stop \
                 (cumulative sums reset at restart — the documented restart-reset)"
            )),
            // The task could not be awaited within the bound: say so.
            None => notifier.note(
                "otlp export: the exporter task did not settle within its stop bound — \
                 it no longer blocks the shutdown; supervision continues",
            ),
        }
    }

    // (6.5) The API stops accepting FIRST (the frozen mount order): graceful
    // shutdown of every listener and a bounded grace for in-flight requests.
    // The open SSE streams end ON the signal (the cooperative close), so the
    // grace normally suffices; a client that STILL holds a connection past
    // it is ABORTED at the grace, and the abort is surfaced (review item 16 /
    // AI-18). The discovery file goes with it (story 21-4): a CLI that reads
    // it from now on starts a new host instead of calling a dead one.
    let aborted = rt.block_on(api.shutdown());
    local::remove_discovery(&state_root, std::process::id());
    notifier.note("control api stopped; no requests are accepted for the rest of the shutdown");
    if aborted {
        notifier.note(
            "control api: a client still held a connection past the graceful window — \
             the connection was aborted (the drain re-issues any interrupted stop, so \
             outcomes converge)",
        );
    }

    // (7) The drain — every RUNNING agent through the bounded ladder, stops
    // issued CONCURRENTLY (one window's wall clock, finding 4), newcomers
    // re-enumerated once (finding 5), every per-instance outcome noted by
    // the drain itself (AI-18) AND returned (the vec is the API return for
    // programmatic callers).
    let _outcomes = rt.block_on(drain::drain_and_stop(&engine, window, &notifier.sink));
    // Review finding 20: the completion notice names what ended the host —
    // the stream reads as one story (X received → ... → complete).
    let label = match ending {
        Ending::Signal(name) => name,
        Ending::Auto(reason) => reason,
    };
    notifier.note(&format!("shutdown complete ({label})"));
    // The engine drops HERE (after the block_on joined): the Drop teardown
    // never races a still-running stop.
    drop(engine);
    drop(rt);
    Ok(())
}

/// Why the host's serve wait ended.
#[derive(Clone, Copy)]
enum Ending {
    /// An OS shutdown signal (its name).
    Signal(&'static str),
    /// The auto-started host's own exit (story 21-4): idle, or the state
    /// root disappeared.
    Auto(&'static str),
}

/// The explicit `--listen` listener's spec, when configured.
fn control_addr(
    listen: Option<std::net::SocketAddr>,
    tls: &Option<Arc<rustls::ServerConfig>>,
) -> Option<control::ListenerSpec> {
    let addr = listen?;
    Some(match tls {
        Some(config) => control::ListenerSpec::Tls(addr, Arc::clone(config)),
        None => control::ListenerSpec::Plain(addr),
    })
}

/// The auto-started host's exit condition (story 21-4, the tmux-server
/// pattern), checked every [`AUTOSTART_CHECK_INTERVAL`]: the state root is
/// gone (a deleted directory — nothing left to serve), or no instance is
/// `starting`/`running`/`paused`/`stopping` AND no request has been in flight
/// or arrived for `window`. A failed fleet read counts as "not idle" — the
/// host never exits on a guess.
async fn autostart_idle_exit(
    engine: Arc<hekma_engine::Engine>,
    activity: control::Activity,
    state_root: PathBuf,
    window: std::time::Duration,
) -> &'static str {
    let mut ticker = tokio::time::interval(AUTOSTART_CHECK_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if !state_root.exists() {
            return "its state root was removed";
        }
        let live = match engine.list().await {
            Ok(instances) => instances.iter().any(|instance| {
                matches!(
                    instance.state,
                    hekma_engine::LifecycleState::Starting
                        | hekma_engine::LifecycleState::Running
                        | hekma_engine::LifecycleState::Paused
                        | hekma_engine::LifecycleState::Stopping
                )
            }),
            Err(_) => true,
        };
        if live {
            continue;
        }
        if matches!(activity.idle_for(), Some(idle) if idle >= window) {
            return "idle: no live agent and no request";
        }
    }
}
