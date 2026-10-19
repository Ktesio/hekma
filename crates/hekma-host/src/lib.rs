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
//! No HTTP here: the Control API is story 17-3 (axum stays out of this tree;
//! verify with `cargo tree -p hekma-host -e normal` = hekma-engine + tokio).
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

mod drain;
mod lock;
mod signals;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub use drain::{drain_and_stop, drain_plan, DrainOutcome, StopResult};
pub use lock::{read_holder_pid, HostLock, LockRefusal};

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
    /// The sink for host + engine notices. `None` (the default) routes both
    /// to stderr, byte-compatible with the engine's no-sink behavior.
    pub diagnostics: Option<DiagnosticSink>,
}

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
        // printed (no-leak discipline).
        f.debug_struct("ServeOptions")
            .field("state_base", &self.state_base)
            .field("stop_window", &self.stop_window)
            .field("metering_listen_port", &self.metering_listen_port)
            .field("diagnostics", &self.diagnostics.as_ref().map(|_| "<sink>"))
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
    /// The caller's options are invalid — today: a stop window outside
    /// [`MIN_STOP_WINDOW_SECS`]..=[`MAX_STOP_WINDOW_SECS`] (review finding
    /// 9: a sub-second or multi-day window is a rejected INVOCATION, not a
    /// failed environment). Usage-class, like the CLI's parse refusal.
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
        diagnostics,
    } = options;
    let has_caller_sink = diagnostics.is_some();
    let notifier = match diagnostics {
        Some(sink) => Notifier::over(sink.clone()),
        None => Notifier::stderr(),
    };

    // (0) The stop window, validated BEFORE anything is touched (review
    // finding 9): an out-of-bounds window is a rejected INVOCATION
    // (`InvalidOptions`), never a 0-second "graceful" ladder discovered at
    // drain time. `None` keeps the engine's DEFAULT_STOP_WINDOW (the single
    // source of the default — never re-hardcoded here).
    let window = std::time::Duration::from_secs(match stop_window {
        None => hekma_engine::domain::DEFAULT_STOP_WINDOW.as_secs(),
        Some(secs) if (MIN_STOP_WINDOW_SECS..=MAX_STOP_WINDOW_SECS).contains(&secs) => secs,
        Some(secs) => {
            let err = ServeError::InvalidOptions {
                message: format!(
                    "invalid stop window {secs}: the graceful window must be a whole \
                     number of seconds from {MIN_STOP_WINDOW_SECS} to \
                     {MAX_STOP_WINDOW_SECS}"
                ),
            };
            if has_caller_sink {
                notifier.note(&err.to_string());
            }
            return Err(err);
        }
    });

    // (1) The state root, via the engine's path authority (the host NEVER
    // joins path segments of the state layout itself — only the engine's own
    // HOST_LOCK_FILE const names the lock file).
    let paths = hekma_engine::paths::EnginePaths::new(state_base.clone()).map_err(|err| {
        ServeError::Startup {
            message: err.to_string(),
        }
    })?;
    let lock_path = paths.state_base().join(hekma_engine::paths::HOST_LOCK_FILE);
    // The host lock runs BEFORE Engine::open, and Engine::open is what would
    // otherwise create the directory — create it here, idempotently.
    std::fs::create_dir_all(paths.state_base()).map_err(|err| ServeError::Startup {
        message: format!(
            "could not create the state directory {}: {err}",
            paths.state_base().display()
        ),
    })?;

    // (2) The single-writer lock. With a caller sink the refusal is noted
    // through it AND returned (AI-18); with none, the caller's own print of
    // the returned error is the single surfacing (no double line).
    let _lock = match HostLock::acquire(&lock_path) {
        Ok(lock) => lock,
        Err(LockRefusal::Held { holder }) => {
            let err = ServeError::StateDirLocked {
                lock_path: lock_path.clone(),
                holder,
            };
            if has_caller_sink {
                notifier.note(&err.to_string());
            }
            return Err(err);
        }
        Err(LockRefusal::Io(err)) => {
            let err = ServeError::Startup {
                message: format!(
                    "could not acquire the host lock {}: {err}",
                    lock_path.display()
                ),
            };
            if has_caller_sink {
                notifier.note(&err.to_string());
            }
            return Err(err);
        }
    };

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
    let (wait_task, early_signal, arming_failure) = rt.block_on(async {
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
    // serve it is always on (a hosted agent inherits ONLY its declared env
    // plus the documented OS-baseline floor). `Arc` because the drain's
    // concurrent stop tasks share it (review finding 4). Opened OUTSIDE any
    // `block_on` — it builds its own internal runtime, exactly as before.
    let engine = Arc::new(
        hekma_engine::Engine::open_with_options(
            state_base.clone(),
            hekma_engine::EngineOptions {
                metering_listen_port,
                child_env_allowlist: true,
                // Route the ENGINE's own diagnostics (adoption notes, the
                // re-bind notes, enforcement breadcrumbs) through the SAME
                // sink the host notices use — one interleaved, honest stream.
                diagnostics: Some(notifier.sink.clone()),
            },
        )
        .map_err(|err| ServeError::Startup {
            message: err.to_string(),
        })?,
    );
    notifier.note(&format!(
        "host started; state root {} — supervising agents until SIGTERM/SIGINT",
        paths.state_base().display()
    ));

    // (6) Resolve the received signal's name — either it already arrived
    // during the arming window or the open (held by the armed streams), or
    // we wait for it now. The streams stay armed through the whole drain
    // (the holder task inside the signal source; review finding 3): a
    // second SIGTERM mid-drain is swallowed and noted, never lethal.
    let received = match early_signal {
        Some(name) => name,
        None => match rt.block_on(wait_task) {
            Ok(Ok(name)) => name,
            Ok(Err(registration)) => {
                return Err(ServeError::Startup {
                    message: registration,
                });
            }
            Err(join) => {
                return Err(ServeError::Startup {
                    message: format!("the shutdown-signal wait task failed: {join}"),
                });
            }
        },
    };
    notifier.note(&format!(
        "{received} received; stopping running agents (graceful window {}s) ...",
        window.as_secs_f32()
    ));

    // (7) The drain — every RUNNING agent through the bounded ladder, stops
    // issued CONCURRENTLY (one window's wall clock, finding 4), newcomers
    // re-enumerated once (finding 5), every per-instance outcome noted by
    // the drain itself (AI-18) AND returned (the vec is the API return for
    // programmatic callers).
    let _outcomes = rt.block_on(drain::drain_and_stop(&engine, window, &notifier.sink));
    // Review finding 20: the completion notice names the signal that ended
    // the host — the stream reads as one story (X received → ... → complete).
    notifier.note(&format!("shutdown complete ({received})"));
    // The engine drops HERE (after the block_on joined): the Drop teardown
    // never races a still-running stop.
    drop(engine);
    drop(rt);
    Ok(())
}
