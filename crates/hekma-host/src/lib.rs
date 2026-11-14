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
mod service_manager;
mod signals;
pub mod wire;

/// The response header that carries a write request's engine notes, and its
/// codec (story 21-4): the `hekma` CLI writes the decoded bytes to its stderr.
pub mod engine_notes {
    pub use crate::control::notes::{decode, encode, ENGINE_NOTES_HEADER};
}

use std::path::{Path, PathBuf};
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
pub use otlp::{Exposition, OtlpExporter, OtlpHeaderValue, OtlpOptions};

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
    /// The shared Metering Proxy's loopback port (story 23-1, the
    /// `--metering-listen` flag): `0` (the default) means the port recorded
    /// for this state directory, else an OS pick that is then recorded —
    /// one stable address for adopted agents and external clients. A taken
    /// port never hops: the host serves on, says which port and why, and
    /// refuses `engine-observed` starts until it is free.
    pub metering_listen_port: u16,
    /// Story 22-5 (`--metering-listen IP:PORT`): the address the metering
    /// proxy binds. `None` (the default, and a bare port) is `127.0.0.1`.
    /// An address beyond loopback requires [`ServeOptions::proxy_tls_cert`]
    /// and [`ServeOptions::proxy_tls_key`]; every call from another machine
    /// must then carry a credential.
    pub metering_listen_addr: Option<std::net::IpAddr>,
    /// Story 22-5 (`--proxy-tls-cert`): the PEM certificate chain the
    /// metering proxy serves beyond loopback (owner-only on Unix).
    pub proxy_tls_cert: Option<PathBuf>,
    /// Story 22-5 (`--proxy-tls-key`): its PEM private key (owner-only on
    /// Unix).
    pub proxy_tls_key: Option<PathBuf>,
    /// Story 22-5 (`--proxy-public-host`): the host name clients beyond
    /// loopback use, shown in the proxy's URLs (display only; without it
    /// the bind address is shown).
    pub proxy_public_host: Option<String>,
    /// Story 23-1 / 21-8 (`--no-identity-headers`, `--identity-headers`):
    /// this run's override of the stored outbound identity-header setting
    /// (`x-hekma-run-id` and `x-hekma-labels`, the run id and labels sent
    /// to the model provider). `None` (the default) follows the stored
    /// setting, on unless an Operator turned it off; `Some(on)` forces it
    /// for this run only and is never stored.
    pub identity_headers: Option<bool>,
    /// Story 22-2 (`--proxy-allowed-hosts`): `Host` header values the
    /// metering proxy accepts beyond the loopback literals (`127.0.0.0/8`,
    /// `::1`, `localhost`). Empty (the default) = the loopback literals
    /// alone; anything else is refused `403 hekma_bad_host` before any dial.
    pub proxy_allowed_hosts: Vec<String>,
    /// Story 22-2 (`--proxy-allowed-origins`): the `Origin` values the
    /// metering proxy accepts. Empty (the default) = a request carrying any
    /// `Origin` is refused (SDKs send none; a browser page does).
    pub proxy_allowed_origins: Vec<String>,
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
    /// Story 21-6's TEST SCALING SEAM for the Control API's set tokens:
    /// `Some(ttl)` overrides the 10-minute lifetime (`Duration::ZERO`
    /// expires every token at once). `None` (every production caller)
    /// keeps 10 minutes. Not an operator surface.
    #[doc(hidden)]
    pub set_token_ttl_override: Option<std::time::Duration>,
    /// Story 24-1: the Shim program supervised processes launch through
    /// (see `EngineOptions::shim_exe`); `None` (the default) holds them
    /// directly. `hekma serve` sets it when `HEKMA_SHIM=1`, a preview
    /// switch until the Shim is on by default.
    pub shim_exe: Option<PathBuf>,
    /// Story 24-2: Handoff Mode — at shutdown, agents held through a Shim
    /// are released (they keep running; the next host reconnects to them)
    /// instead of stopped. Off by default; agents held directly are always
    /// stopped.
    pub handoff: bool,
    /// Story 24-3: whether the Operator chose the mode (a flag or the
    /// environment) rather than taking the default. Only a chosen "off"
    /// asks for a crashed host's agents to be cleaned up.
    pub handoff_chosen: bool,
    /// Story 24-2: this host is the successor of an in-place upgrade — the
    /// previous host's handoff channel, which hands over the metering
    /// proxy's socket and the host lock (instead of acquiring it).
    pub handoff_from: Option<PathBuf>,
    /// Story 24-2: the one-time token the previous host passed in
    /// `HEKMA_HANDOFF_TOKEN` (required with `handoff_from`).
    pub handoff_token: Option<String>,
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
            .field("metering_listen_addr", &self.metering_listen_addr)
            .field("proxy_tls_cert", &self.proxy_tls_cert)
            .field("proxy_tls_key", &self.proxy_tls_key)
            .field("proxy_public_host", &self.proxy_public_host)
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
        metering_listen_addr,
        proxy_tls_cert,
        proxy_tls_key,
        proxy_public_host,
        identity_headers,
        proxy_allowed_hosts,
        proxy_allowed_origins,
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
        set_token_ttl_override,
        shim_exe,
        handoff,
        handoff_chosen,
        handoff_from,
        handoff_token,
    } = options;
    let shim_launches = shim_exe.is_some();
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
            || tls_key.is_some()
            || proxy_tls_cert.is_some()
            || proxy_tls_key.is_some()
            || proxy_public_host.is_some()
            || metering_listen_addr.is_some_and(|addr| !addr.is_loopback()))
    {
        return Err(refuse(ServeError::InvalidOptions {
            message: "--autostart refuses the explicit-listener flags (--listen, \
                      --bearer-secret, --tls-cert, --tls-key, --proxy-tls-cert, \
                      --proxy-tls-key, --proxy-public-host, and a --metering-listen address \
                      beyond loopback): an auto-started host serves only on loopback"
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
    // (0.3) Story 22-5: the metering proxy beyond loopback. Its own PEM
    // pair (both or neither), only with a bind beyond loopback, and
    // REQUIRED there; read and checked (owner-only) before the lock.
    let proxy_beyond_loopback = metering_listen_addr.is_some_and(|addr| !addr.is_loopback());
    let proxy_tls = match (&proxy_tls_cert, &proxy_tls_key) {
        (None, None) => None,
        (Some(_), None) | (None, Some(_)) => {
            return Err(refuse(ServeError::InvalidOptions {
                message: "--proxy-tls-cert and --proxy-tls-key go together: pass both PEM \
                          files (or neither, for a loopback metering proxy)"
                    .to_string(),
            }));
        }
        (Some(cert), Some(key)) => {
            if !proxy_beyond_loopback {
                return Err(refuse(ServeError::InvalidOptions {
                    message: "--proxy-tls-cert/--proxy-tls-key require a --metering-listen \
                              address beyond loopback (for example 0.0.0.0:8400): on \
                              loopback the metering proxy serves plain HTTP"
                        .to_string(),
                }));
            }
            match control::tls::load_server_config(cert, key) {
                Ok(config) => Some(hekma_engine::ProxyTls(config)),
                Err(message) => return Err(refuse(ServeError::InvalidOptions { message })),
            }
        }
    };
    if let Some(addr) = metering_listen_addr.filter(|_| proxy_beyond_loopback) {
        if proxy_tls.is_none() {
            return Err(refuse(ServeError::InvalidOptions {
                message: format!(
                    "the metering proxy cannot bind {addr} beyond loopback without transport \
                     encryption: pass --proxy-tls-cert and --proxy-tls-key (PEM files), or \
                     bind 127.0.0.1"
                ),
            }));
        }
    }
    if proxy_public_host.is_some() && !proxy_beyond_loopback {
        return Err(refuse(ServeError::InvalidOptions {
            message: "--proxy-public-host requires a --metering-listen address beyond \
                      loopback: it names the host clients on other machines use"
                .to_string(),
        }));
    }
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
    // Story 24-2: a successor takes the previous host's lock over the
    // handoff channel (the same lock, never a second acquisition) along with
    // the metering proxy's listening socket.
    let mut joined: Option<hekma_engine::handoff::Joined> = None;
    let acquired = match &handoff_from {
        Some(channel) => {
            let token = handoff_token.as_deref().ok_or_else(|| {
                refuse(ServeError::InvalidOptions {
                    message: format!(
                        "--handoff-from needs the token the previous host passes in {}",
                        hekma_engine::handoff::HANDOFF_TOKEN_ENV
                    ),
                })
            })?;
            let mut taken = hekma_engine::handoff::join(
                channel,
                token,
                hekma_engine::handoff::SUCCESSOR_READY_BOUND,
            )
            .map_err(|err| {
                refuse(ServeError::Startup {
                    message: format!("could not take over from the previous host: {err}"),
                })
            })?;
            let file = taken.take_lock().ok_or_else(|| {
                refuse(ServeError::Startup {
                    message: "the previous host handed over no host lock".to_string(),
                })
            })?;
            joined = Some(taken);
            Ok(HostLock::adopt(file, &lock_path))
        }
        None => HostLock::acquire(&lock_path),
    };
    let _lock = match acquired {
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
    // Story 22-2 (P10): the engine's custody cache will hold provider keys
    // in this process's memory — keep it out of core dumps (and, on Linux,
    // out of same-user ptrace) before the engine opens. A failure is
    // surfaced and serving goes on.
    if let Err(detail) = hekma_engine::harden_host_memory() {
        notifier.note(&format!(
            "the Service Host could not harden its memory against core dumps: {detail}"
        ));
    }
    let note_tap = if autostart {
        control::NoteTap::keeping_startup_notes()
    } else {
        control::NoteTap::default()
    };
    let engine = Arc::new(
        hekma_engine::Engine::open_with_options(
            state_base.clone(),
            hekma_engine::EngineOptions {
                // Story 23-1: the shared Metering Proxy — sticky per state
                // directory (the flag's port, else the recorded one, else an
                // OS pick that is then recorded), with the host's
                // identity-header posture. A taken port is surfaced by the
                // engine's open note (the host keeps serving; engine-observed
                // starts are refused until the port is free).
                metering_proxy: hekma_engine::MeteringProxyOptions {
                    enabled: true,
                    port: metering_listen_port,
                    // Story 22-5: the bind and its TLS (beyond loopback).
                    bind: metering_listen_addr,
                    tls: proxy_tls,
                    public_host: proxy_public_host,
                    sticky: true,
                    identity_headers,
                    // Story 22-2: the Host/Origin guard's Operator lists.
                    allowed_hosts: proxy_allowed_hosts,
                    allowed_origins: proxy_allowed_origins,
                    // Story 24-2: a successor serves on the socket the
                    // previous host kept open (no connection refused).
                    inherited_listener: joined
                        .as_mut()
                        .and_then(|joined| joined.take_listener())
                        .map(hekma_engine::InheritedListener::new)
                        .unwrap_or_default(),
                    ..hekma_engine::MeteringProxyOptions::default()
                },
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
                // Story 24-1: the Shim program, when the host was given one.
                shim_exe,
                // Story 24-3: this host owns the data path; its Handoff Mode
                // is recorded before anything is adopted.
                host_posture: Some(match (handoff, handoff_chosen) {
                    (true, _) => hekma_engine::HandoffPosture::On,
                    (false, true) => hekma_engine::HandoffPosture::Off,
                    (false, false) => hekma_engine::HandoffPosture::Unset,
                }),
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
    // Story 23-1: the shared Metering Proxy's URL rides the started notice
    // beside the state root — the address every metered agent and binding
    // client points at. (A bind failure was already surfaced by the
    // engine's open note; this stays silent then.)
    let proxy_info = engine.metering_proxy();
    let proxy_url = proxy_info.as_ref().map(|info| info.url.clone());
    // Story 22-5: beyond loopback, the address other machines use (the
    // public host when given, else the bind address).
    let proxy_public_url = proxy_info.as_ref().and_then(|info| info.public_url.clone());
    match (&proxy_url, &proxy_public_url) {
        (Some(_), Some(public)) => {
            notifier.note(&format!(
                "the metering proxy is listening on {public}. Every call to it from another \
                 machine must carry a credential."
            ));
            if !engine.proxy_access_active() {
                notifier.note(
                    "no proxy-access credential is active yet, so every call from other \
                     machines will be refused until you issue one (hekma binding credential \
                     issue <name> --class proxy-access).",
                );
            }
        }
        (Some(url), None) => {
            notifier.note(&format!("the metering proxy is listening on {url}."));
        }
        _ => {}
    }
    if proxy_url.is_some() {
        notifier.note(&port_squatting_note(hekma_engine::interactive_users_hint()));
    }
    // Story 21-8: a serve flag that overrides the stored identity-header
    // setting says so, so the per-run override is never silent.
    if let Some(note) = engine
        .blocking()
        .proxy_settings()
        .ok()
        .and_then(|settings| identity_headers_note(&settings))
    {
        notifier.note(&note);
    }

    // Story 24-3: what the previous owner left behind. A crashed host
    // with Handoff Mode off asked for its agents to be cleaned up: each is
    // stopped the ordinary way and its Run ends `engine-crash`. On, or not
    // recorded: they were adopted and keep running.
    if let Some(recovery) = engine.host_recovery() {
        let (cleaned, failed) = match recovery.previous {
            hekma_engine::PreviousHost::Gone { handoff: false } => {
                let mut cleaned = Vec::new();
                let mut failed = Vec::new();
                for name in &recovery.adopted {
                    match engine.blocking().clean_up_after_crash(name, Some(window)) {
                        Ok(_) => cleaned.push(name.clone()),
                        Err(err) => failed.push(format!("{name} ({err})")),
                    }
                }
                (cleaned, failed)
            }
            _ => (Vec::new(), Vec::new()),
        };
        if let Some(note) = recovery_note(&recovery, &cleaned, &failed) {
            notifier.note(&note);
        }
    }

    // Story 24-2: say what a shutdown will do to the agents, and whether
    // the service manager would take them down with this host anyway.
    if let Some(note) = handoff_note(handoff, shim_launches) {
        notifier.note(note);
    }
    let handoff_check = service_manager::check();
    if shim_launches {
        notifier.note(&handoff_check.note());
    }
    let host_doc = serde_json::json!({
        "pid": std::process::id(),
        "hekma_version": build_version.clone().unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
        "handoff": handoff,
        "handoff_chosen": handoff_chosen,
        "shim": shim_launches,
        "handoff_check": handoff_check.as_str(),
        "handoff_check_detail": handoff_check.detail(),
    });

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
        // Story 23-1: where the shared Metering Proxy listens (absent when
        // it could not bind — `GET /v0/proxy` and the open note say why).
        metering_proxy_url: proxy_url,
        metering_proxy_public_url: proxy_public_url,
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
        set_token_ttl_override,
        host_doc,
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
                // Story 24-5: the exposition and its label allow-list, the
                // collector's CA root, and the engine's counters sampled at
                // each flush.
                exposition: otlp_options.effective_exposition(),
                label_keys: otlp_options.label_keys.clone(),
                label_cardinality: otlp_options.effective_label_cardinality(),
                ca_file: otlp_options.ca_file.clone(),
                counters: Some({
                    let engine = Arc::clone(&engine);
                    std::sync::Arc::new(move || engine.export_counters())
                }),
            };
            let rx = engine.subscribe();
            let exporter =
                rt.block_on(async { otlp::OtlpExporter::start(rx, config, notifier.sink.clone()) });
            Some(exporter)
        }
    };

    // Story 24-2: a successor now serves — tell the previous host, which
    // exits on it.
    if let Some(joined) = joined.take() {
        let reattached = rt
            .block_on(engine.fleet())
            .map(|fleet| drain_plan(&fleet).len())
            .unwrap_or(0);
        match joined.ready(reattached) {
            Ok(()) => notifier.note(&format!(
                "took over from the previous host in place; {reattached} agent(s) reconnected"
            )),
            Err(err) => notifier.note(&format!(
                "took over, but the previous host could not be told ({err}); it exits when its \
                 wait ends"
            )),
        }
    }

    // (6) Resolve why the host ends — the received signal (it may already
    // have arrived during the arming window or the open, held by the armed
    // streams), or, in AUTO mode, idleness / a vanished state root. The
    // streams stay armed through the whole drain (the holder task inside the
    // signal source; review finding 3): a second SIGTERM mid-drain is
    // swallowed and noted, never lethal.
    let upgrade_requests = api.upgrade_requests();
    let mut upgrade_link: Option<hekma_engine::handoff::Link> = None;
    let ending: Ending = match early_signal {
        Some(name) => Ending::Signal(name),
        None => loop {
            let upgrade_wait = upgrade_requests.notified();
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
                        () = upgrade_wait => Ok(Ending::Upgrade),
                    }
                })
            } else {
                rt.block_on(async {
                    tokio::select! {
                        signaled = signal_wait => signaled,
                        () = upgrade_wait => Ok(Ending::Upgrade),
                    }
                })
            };
            match outcome {
                // Story 24-2: launch the successor and wait for its hello;
                // until then nothing has stopped, so a failure here keeps
                // this host serving.
                Ok(Ending::Upgrade) => match begin_upgrade(
                    &state_root,
                    std::env::current_exe(),
                    hekma_engine::handoff::SUCCESSOR_CONNECT_BOUND,
                ) {
                    Ok(link) => {
                        notifier.note(&format!(
                            "upgrading in place: the successor (pid {}) connected; handing \
                             over",
                            link.successor_pid()
                        ));
                        upgrade_link = Some(link);
                        break Ending::Upgrade;
                    }
                    Err(message) => {
                        notifier.note(&format!(
                            "the in-place upgrade did not start ({message}); this host keeps \
                             serving"
                        ));
                        continue;
                    }
                },
                Ok(ending) => break ending,
                Err(err) => {
                    let _ = rt.block_on(api.shutdown());
                    local::remove_discovery(&state_root, std::process::id());
                    return Err(err);
                }
            }
        },
    };
    let what = if handoff || matches!(ending, Ending::Upgrade) {
        "releasing agents in their own Shim and stopping the rest"
    } else {
        "stopping running and paused agents"
    };
    match ending {
        Ending::Upgrade => notifier.note(&format!(
            "in-place upgrade; {what} (graceful window {}s) ...",
            window.as_secs_f32()
        )),
        Ending::Signal(name) => notifier.note(&format!(
            "{name} received; {what} (graceful window {}s) ...",
            window.as_secs_f32()
        )),
        Ending::Auto(reason) => notifier.note(&format!(
            "auto-started host exiting ({reason}); {what} (graceful window {}s) ...",
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
    // Story 24-2: an upgrade leaves the discovery file for the successor to
    // rewrite (removing it would let a CLI start a third host meanwhile).
    if upgrade_link.is_none() {
        local::remove_discovery(&state_root, std::process::id());
    }
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
    // Story 24-2: the in-place upgrade — release, keep the proxy's socket,
    // close the engine (its store writer with it), hand the socket and the
    // lock over, and exit once the successor serves.
    if let Some(link) = upgrade_link {
        let (handed, listener) = engine
            .blocking()
            .shutdown_for_upgrade(window, Some(&notifier.sink));
        if !handed.released.is_empty() {
            notifier.note(&format!(
                "{} agent(s) left running in their Shims for the successor: {}",
                handed.released.len(),
                handed.released.join(", ")
            ));
        }
        close_engine(engine, std::time::Duration::from_secs(10));
        let outcome = hand_over(link, listener.as_ref(), _lock.file());
        drop(rt);
        let (note, result) = upgrade_outcome(outcome);
        notifier.note(&note);
        return result;
    }
    // Story 24-2: in Handoff Mode, agents held through a Shim are released
    // (they keep running; the next host reconnects), the rest drain.
    if handoff {
        let handed = engine
            .blocking()
            .shutdown_for_handoff(window, Some(&notifier.sink));
        if !handed.released.is_empty() {
            notifier.note(&format!(
                "{} agent(s) left running in their Shims for the next host: {}",
                handed.released.len(),
                handed.released.join(", ")
            ));
        }
    } else {
        let _outcomes = rt.block_on(drain::drain_and_stop(&engine, window, &notifier.sink));
    }
    // Review finding 20: the completion notice names what ended the host —
    // the stream reads as one story (X received → ... → complete).
    let label = match ending {
        Ending::Signal(name) => name,
        Ending::Auto(reason) => reason,
        Ending::Upgrade => "upgrade",
    };
    notifier.note(&format!("shutdown complete ({label})"));
    // The engine drops HERE (after the block_on joined): the Drop teardown
    // never races a still-running stop.
    drop(engine);
    drop(rt);
    Ok(())
}

/// Story 24-2: open the handoff channel, launch the successor — this
/// binary, with this process's own arguments plus `--handoff-from`, the
/// token in its environment only — and wait for its checked hello.
fn begin_upgrade(
    state_root: &Path,
    exe: std::io::Result<PathBuf>,
    bound: std::time::Duration,
) -> Result<hekma_engine::handoff::Link, String> {
    use hekma_engine::handoff;
    if !handoff::supported() {
        return Err("an in-place upgrade is not available on this operating system yet".into());
    }
    let channel = handoff::Predecessor::open(state_root).map_err(|err| err.to_string())?;
    let exe = exe.map_err(|err| format!("this binary's path: {err}"))?;
    let mut child = std::process::Command::new(exe)
        .args(successor_args(std::env::args_os().skip(1), channel.path()))
        .env(handoff::HANDOFF_TOKEN_ENV, channel.token())
        .spawn()
        .map_err(|err| format!("the successor could not be launched: {err}"))?;
    match channel.await_successor(bound) {
        Ok(link) => Ok(link),
        Err(err) => {
            // It never took anything over: end it.
            let _ = child.kill();
            let _ = child.wait();
            Err(err.to_string())
        }
    }
}

/// The successor's arguments: this process's own, without an earlier
/// `--handoff-from <channel>`, plus the new channel.
fn successor_args(
    args: impl Iterator<Item = std::ffi::OsString>,
    channel: &Path,
) -> Vec<std::ffi::OsString> {
    let mut out = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        if arg == "--handoff-from" {
            args.next();
            continue;
        }
        out.push(arg);
    }
    out.push("--handoff-from".into());
    out.push(channel.as_os_str().to_owned());
    out
}

/// Drop the engine once nothing else holds it (its store writer closes
/// with it, before the successor opens its own). The Control API and the
/// exporter released their handles at their shutdown; a straggler is
/// waited for briefly.
fn close_engine(engine: Arc<hekma_engine::Engine>, bound: std::time::Duration) {
    let mut engine = engine;
    let deadline = std::time::Instant::now() + bound;
    loop {
        match Arc::try_unwrap(engine) {
            Ok(engine) => {
                drop(engine);
                return;
            }
            Err(shared) if std::time::Instant::now() < deadline => {
                engine = shared;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(shared) => {
                drop(shared);
                return;
            }
        }
    }
}

/// The predecessor's last note and its exit, from the hand-over's outcome.
fn upgrade_outcome(outcome: Result<(u32, usize), String>) -> (String, Result<(), ServeError>) {
    match outcome {
        Ok((pid, reattached)) => (
            format!(
                "handed over to the successor (pid {pid}, {reattached} agent(s) reconnected); \
                 shutdown complete (upgrade)"
            ),
            Ok(()),
        ),
        Err(message) => (
            format!(
                "the successor did not finish taking over ({message}); agents in their Shims \
                 keep running, and the next hekma write starts a host that reconnects to them"
            ),
            Err(ServeError::Startup {
                message: format!("the in-place upgrade did not complete: {message}"),
            }),
        ),
    }
}

/// Hand the socket and the lock over and wait for the successor's `ready`.
fn hand_over(
    mut link: hekma_engine::handoff::Link,
    listener: Option<&std::net::TcpListener>,
    lock: &std::fs::File,
) -> Result<(u32, usize), String> {
    link.offer(listener, lock).map_err(|err| err.to_string())?;
    link.await_ready(hekma_engine::handoff::SUCCESSOR_READY_BOUND)
        .map_err(|err| err.to_string())
}

/// Story 24-3: the started notice's line about the previous owner's agents.
fn recovery_note(
    recovery: &hekma_engine::HostRecovery,
    cleaned: &[String],
    failed: &[String],
) -> Option<String> {
    use hekma_engine::PreviousHost;
    let adopted = recovery.adopted.join(", ");
    match recovery.previous {
        PreviousHost::Gone { handoff: false } if !recovery.adopted.is_empty() => {
            let mut note = format!(
                "the previous host stopped without Handoff Mode and left {} agent(s) running; \
                 stopped {} of them ({}); their Runs end with cause engine-crash",
                recovery.adopted.len(),
                cleaned.len(),
                cleaned.join(", ")
            );
            if !failed.is_empty() {
                note.push_str(&format!(
                    "; could not stop: {} — they stay supervised here",
                    failed.join(", ")
                ));
            }
            Some(note)
        }
        PreviousHost::Gone { handoff: true } if !recovery.adopted.is_empty() => Some(format!(
            "the previous host ran in Handoff Mode; reconnected to its {} agent(s): {adopted}",
            recovery.adopted.len()
        )),
        PreviousHost::Unknown if !recovery.adopted.is_empty() => Some(format!(
            "no record of the previous host's Handoff Mode, so its {} agent(s) were adopted, not \
             stopped: {adopted}",
            recovery.adopted.len()
        )),
        _ => None,
    }
}

/// Story 24-2: the started notice's Handoff Mode line, when there is one.
fn handoff_note(handoff: bool, shim_launches: bool) -> Option<&'static str> {
    match (handoff, shim_launches) {
        (true, true) => Some(
            "Handoff Mode is on: agents in their own Shim keep running when this host stops, \
             and the next host reconnects to them",
        ),
        (true, false) => Some(
            "Handoff Mode is on, but agents are not launched in their own Shim (set \
             HEKMA_SHIM=1), so every agent is still stopped when this host stops",
        ),
        _ => None,
    }
}

/// Why the host's serve wait ended.
#[derive(Clone, Copy)]
enum Ending {
    /// An OS shutdown signal (its name).
    Signal(&'static str),
    /// Story 24-2: an Operator asked for an in-place upgrade.
    Upgrade,
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

/// Parse a `--metering-listen` value (story 22-5): a bare port keeps the
/// loopback meaning (`127.0.0.1`), `IP:PORT` names the address (`[IPv6]:PORT`
/// for IPv6). A host name is refused: the proxy binds an address, and only
/// an IP literal says which one. Shared by `hekma serve` and `hekma-host`.
pub fn parse_metering_listen(raw: &str) -> Result<(Option<std::net::IpAddr>, u16), String> {
    let raw = raw.trim();
    if let Ok(port) = raw.parse::<u16>() {
        return Ok((None, port));
    }
    if let Ok(addr) = raw.parse::<std::net::SocketAddr>() {
        return Ok((Some(addr.ip()), addr.port()));
    }
    Err(format!(
        "--metering-listen {raw:?} is not a port or an IP:PORT address: pass a port (0-65535), \
         or an IP literal and a port such as 0.0.0.0:8400 or [::]:8400 (a host name is not \
         accepted)"
    ))
}

/// The started notice's port-squatting line (story 22-5): whenever the
/// proxy binds, since its sticky port is what another user could take
/// while no host holds it.
pub(crate) fn port_squatting_note(other_users: Option<usize>) -> String {
    match other_users {
        Some(0) => "this machine has no other user accounts that can log in, so no other user \
                    can listen on the metering proxy's port while Hekma is not running."
            .to_string(),
        Some(_) => "this machine has other user accounts that can log in. While Hekma is not \
                    running, another user could listen on the metering proxy's port and \
                    collect what is sent to it. Use socket activation (systemd or launchd), run \
                    Hekma on a machine you do not share, or bind a privileged port where the \
                    operating system allows one."
            .to_string(),
        None => "Hekma cannot tell whether this machine has other interactive users. If it \
                 does, another user could listen on the metering proxy's port while Hekma is \
                 not running; see the metering proxy docs on port squatting."
            .to_string(),
    }
}

/// The started notice's line when this run's serve flag disagrees with the
/// stored identity-header setting (story 21-8).
fn identity_headers_note(settings: &hekma_engine::ProxySettings) -> Option<String> {
    let word = |on: bool| if on { "on" } else { "off" };
    (settings.forced_by_flag && settings.identity_headers != settings.stored_identity_headers).then(
        || {
            format!(
                "outbound identity headers are {} for this run (a serve flag); the stored \
                 setting is {}, and the next start without the flag uses it.",
                word(settings.identity_headers),
                word(settings.stored_identity_headers)
            )
        },
    )
}

#[cfg(test)]
mod identity_headers_note_tests {
    use super::identity_headers_note;
    use hekma_engine::ProxySettings;

    #[test]
    fn the_started_notice_names_a_flag_that_disagrees_with_the_stored_setting() {
        let settings = |identity_headers, stored_identity_headers, forced_by_flag| ProxySettings {
            identity_headers,
            stored_identity_headers,
            forced_by_flag,
        };
        let note = identity_headers_note(&settings(false, true, true)).unwrap();
        assert!(note.contains("off for this run"), "{note}");
        assert!(note.contains("stored setting is on"), "{note}");
        let note = identity_headers_note(&settings(true, false, true)).unwrap();
        assert!(
            note.contains("on for this run") && note.contains("is off"),
            "{note}"
        );
        // A flag that agrees, or no flag, says nothing.
        assert!(identity_headers_note(&settings(true, true, true)).is_none());
        assert!(identity_headers_note(&settings(false, false, false)).is_none());
    }

    #[test]
    fn the_port_squatting_line_says_what_the_account_list_shows() {
        let others = super::port_squatting_note(Some(2));
        assert!(
            others.contains("other user accounts that can log in"),
            "{others}"
        );
        assert!(others.contains("socket activation"), "{others}");
        assert!(super::port_squatting_note(Some(0)).contains("no other user accounts"));
        assert!(super::port_squatting_note(None).contains("cannot tell"));
    }

    #[test]
    fn the_handoff_note_says_what_a_shutdown_will_do() {
        assert!(super::handoff_note(false, true).is_none());
        assert!(super::handoff_note(false, false).is_none());
        assert!(super::handoff_note(true, true)
            .unwrap()
            .contains("keep running when this host stops"));
        assert!(super::handoff_note(true, false)
            .unwrap()
            .contains("every agent is still stopped"));
    }

    #[test]
    fn the_successor_replays_this_hosts_arguments_with_the_new_channel() {
        let args = ["serve", "--handoff-from", "/old.sock", "--autostart"]
            .into_iter()
            .map(std::ffi::OsString::from);
        let out = super::successor_args(args, std::path::Path::new("/new.sock"));
        assert_eq!(
            out,
            ["serve", "--autostart", "--handoff-from", "/new.sock"]
                .map(std::ffi::OsString::from)
                .to_vec()
        );
    }

    #[test]
    fn an_upgrade_whose_successor_never_connects_keeps_this_host_and_reports_why() {
        if !hekma_engine::handoff::supported() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let bound = std::time::Duration::from_millis(400);
        let err =
            super::begin_upgrade(state.path(), Ok("/usr/bin/true".into()), bound).unwrap_err();
        assert!(err.contains("connected"), "{err}");
        let err =
            super::begin_upgrade(state.path(), Ok(state.path().join("no-such-binary")), bound)
                .unwrap_err();
        assert!(err.contains("could not be launched"), "{err}");
        let err = super::begin_upgrade(state.path(), Err(std::io::Error::other("unknown")), bound)
            .unwrap_err();
        assert!(err.contains("this binary's path"), "{err}");
    }

    #[test]
    fn the_hand_overs_outcome_is_the_predecessors_last_word() {
        let (note, result) = super::upgrade_outcome(Ok((42, 3)));
        assert!(note.contains("pid 42, 3 agent(s)"), "{note}");
        assert!(result.is_ok());
        let (note, result) = super::upgrade_outcome(Err("it died".to_string()));
        assert!(
            note.contains("it died") && note.contains("keep running"),
            "{note}"
        );
        assert!(matches!(result, Err(super::ServeError::Startup { .. })));
    }

    #[test]
    fn closing_waits_for_a_straggling_handle_only_within_its_bound() {
        let state = tempfile::tempdir().unwrap();
        let engine = std::sync::Arc::new(
            hekma_engine::Engine::open(Some(state.path().to_path_buf())).unwrap(),
        );
        let straggler = std::sync::Arc::clone(&engine);
        let started = std::time::Instant::now();
        super::close_engine(engine, std::time::Duration::from_millis(100));
        assert!(started.elapsed() >= std::time::Duration::from_millis(100));
        // Alone, it closes at once.
        super::close_engine(straggler, std::time::Duration::from_secs(10));
    }

    #[test]
    fn the_recovery_note_says_what_happened_to_the_previous_hosts_agents() {
        use hekma_engine::{HostRecovery, PreviousHost};
        let recovery = |previous, adopted: &[&str]| HostRecovery {
            previous,
            adopted: adopted.iter().map(|name| name.to_string()).collect(),
        };
        let off = recovery(PreviousHost::Gone { handoff: false }, &["a", "b"]);
        let note = super::recovery_note(&off, &["a".into()], &["b (busy)".into()]).unwrap();
        assert!(
            note.contains("left 2 agent(s) running; stopped 1 of them (a)"),
            "{note}"
        );
        assert!(note.contains("engine-crash") && note.contains("could not stop: b (busy)"));
        let on = recovery(PreviousHost::Gone { handoff: true }, &["a"]);
        assert!(super::recovery_note(&on, &[], &[])
            .unwrap()
            .contains("reconnected to its 1"));
        let unknown = recovery(PreviousHost::Unknown, &["a"]);
        assert!(super::recovery_note(&unknown, &[], &[])
            .unwrap()
            .contains("adopted, not stopped"));
        assert!(super::recovery_note(&recovery(PreviousHost::Live, &["a"]), &[], &[]).is_none());
        assert!(super::recovery_note(
            &recovery(PreviousHost::Gone { handoff: false }, &[]),
            &[],
            &[]
        )
        .is_none());
    }
}
