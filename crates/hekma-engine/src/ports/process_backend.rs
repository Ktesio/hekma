//! The [`ProcessBackend`] port (hexagonal, spine AD-1/AD-4).
//!
//! All process control — spawning an agent, stopping it, checking whether it is
//! still alive — enters the engine through this ONE port. Its methods speak
//! DOMAIN terms ([`SpawnSpec`], [`StopOutcome`], [`ProcessStatus`]), never OS
//! syscalls. The per-OS implementations live behind it in
//! `crates/hekma-engine/src/backends/{unix,windows}/` — the sole allowlisted
//! home for `#[cfg]` OS attributes (the OS-cfg CI gate). This file, and every
//! other engine module except `backends/`, is cfg-free.
//!
//! ## Sync trait, blocking pool (documented choice)
//!
//! The trait methods are **synchronous** thin syscall wrappers. The async
//! [`Engine`](crate::Engine) calls them from tokio's blocking pool
//! (`spawn_blocking`), so a blocking syscall (or the bounded graceful-stop wait)
//! never stalls an async worker. Keeping the trait sync keeps each backend a
//! minimal syscall shim and keeps the OS-specific surface as small as possible.
//!
//! ## Associated `Handle` (why not `Box<dyn>`)
//!
//! Each backend owns its running-process resources through an associated
//! [`ProcessBackend::Handle`] type (on Unix: the child + its process-group id;
//! on Windows: the child + its Job Object). Exactly one backend is compiled per
//! target (cfg-selected in `backends/mod.rs`), so the supervisor names the
//! concrete handle through a cfg-selected type alias — no trait objects, no
//! dynamic dispatch, and the port stays free of OS types.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use thiserror::Error;

use crate::domain::{LogLine, LogStream};
use crate::time::now_rfc3339;

/// The resolved launch of an Agent Instance (built from a manifest `OpTemplate`
/// or a native adapter). Everything the backend needs to spawn the process, in
/// domain terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnSpec {
    /// The executable to run (a program name resolved on `PATH`, or a path).
    pub exec: String,
    /// Positional arguments.
    pub args: Vec<String>,
    /// Environment overrides applied on top of the inherited environment.
    pub env: BTreeMap<String, String>,
    /// The working directory for the child — the Agent Home.
    pub working_dir: PathBuf,
    /// A file the child's STDOUT ALONE is redirected to — the legacy,
    /// Epic-3-critical `agent.log` (AD-12). Fix pass (review of #80): this
    /// is a DIRECT OS-level redirect (`Stdio::from(file)`), the SAME
    /// crash-immune mechanism used before story 4-2 ever piped anything —
    /// the child's `write()` to stdout succeeds or fails based ONLY on this
    /// regular file, NEVER on whether the engine process is even still
    /// alive to read anything. Story 4-2 (pre-fix) combined stdout+stderr
    /// into this ONE file via engine-side reader threads draining a pipe;
    /// that coupled the agent's write-success to the engine's liveness for
    /// the first time (NFR-1 regression) and, separately, made
    /// `drain_usage_for`'s billing-critical sentinel read depend on a
    /// reader thread's OWN schedule (a genuinely new race, H2). Reverting to
    /// a direct per-STREAM redirect (see [`SpawnSpec::stderr_log_file`] for
    /// the other stream) closes BOTH: `agent.log`'s CONTENT stays raw,
    /// unattributed, byte-identical to today's format (only stdout, since
    /// the `KTESIO_USAGE` sentinel `drain_usage_for` parses is a
    /// stdout-only convention — confirmed via `docs/manifest.md`/
    /// `docs/architecture.md`/`ports::usage_source` — so nothing
    /// billing-critical is lost by not ALSO merging stderr into this file
    /// directly; stderr is captured separately, see below). `None` inherits
    /// the engine's streams (used only in tests that do not assert captured
    /// output).
    pub log_file: Option<PathBuf>,
    /// Where the ATTRIBUTED, rotated capture (`agent-out`/`agent-err`/`engine`
    /// lines) should be written — the CURRENT generation file (story 4-2,
    /// AD-12). `Some` in every PRODUCTION spawn, paired 1:1:1 with
    /// `log_file`/`stderr_log_file` (the supervisor computes all three from
    /// the SAME `Registry` path authority in the same breath) — capture is
    /// UNCONDITIONAL and capability-independent (AC-E), never gated on
    /// `Capability::Interaction` the way [`SpawnSpec::pipe_stdin`] gates the
    /// stdin *write* direction. `None` only alongside `log_file: None`, the
    /// small set of unit tests that assert nothing about captured output —
    /// this pairing is a narrow test-fixture convenience, not a capability
    /// gate: reading FROM a process is never gated, only writing TO it is.
    ///
    /// Fix pass (review of #80): this file is no longer fed by a live pipe
    /// either — it is populated by a background TAILER that incrementally
    /// reads the two crash-immune raw files ([`SpawnSpec::log_file`],
    /// [`SpawnSpec::stderr_log_file`]) and re-attributes+rotates their
    /// content, so it can lag or stop entirely if the engine crashes
    /// without harming the agent's OWN writes at all — it is now a derived,
    /// best-effort VIEW, never a dependency of the agent's liveness.
    pub attributed_log_path: Option<PathBuf>,
    /// A file the child's STDERR ALONE is redirected to (fix pass — review of
    /// #80) — the crash-immune raw stderr capture, direct OS write, exactly
    /// like [`SpawnSpec::log_file`] but for the other stream. `Some` in
    /// every PRODUCTION spawn, paired 1:1:1 with `log_file`/
    /// `attributed_log_path` (all three `Some` together, or all `None` —
    /// see [`SpawnSpec::attributed_log_path`]'s docs on the narrow
    /// test-fixture `None` convenience). See the module docs above the
    /// capture primitives for why stdout and stderr each get their OWN
    /// direct file rather than sharing one.
    pub stderr_log_file: Option<PathBuf>,
    /// The Agent Instance name, stamped on every captured [`LogLine`]
    /// (`LogLine.instance`) — paired with [`SpawnSpec::attributed_log_path`].
    pub instance_name: String,
    /// Whether to pipe the child's stdin (story 4.1 fix pass, HIGH finding —
    /// review of #79).
    ///
    /// `true` only when the instance's declared `Capability::Interaction`
    /// level on the CURRENT OS is `Guaranteed` or `BestEffort` — the caller
    /// (the supervisor, at spawn time) reads the effective Capability
    /// Declaration and sets this field; the backends themselves stay
    /// capability-agnostic (dumb process executors), mirroring how this
    /// codebase already gates BEHAVIOR (not just callability) on declared
    /// capabilities elsewhere (e.g. pause's SIGSTOP-vs-noop branching).
    ///
    /// `false` uses the pre-story-4.1 safe default (`Stdio::null()`): an
    /// adapter that declares no interaction support behaves EXACTLY as
    /// before spawn ever piped anything. This matters because the story's
    /// original implementation piped stdin UNCONDITIONALLY for every
    /// spawned process: a process that does a blocking read of stdin at
    /// startup (a common "sniff for piped input" real-CLI idiom) then hangs
    /// forever, because the engine holds the pipe's write end open for the
    /// process's entire supervised lifetime and nothing ever writes to it
    /// unless `send` is called — the child never sees EOF and never
    /// unblocks, yet is reported `running` (readiness here is just "the
    /// process didn't exit immediately"), a silent deadlock with no error
    /// signal anywhere.
    ///
    /// Story 12-1 note: the supervisor FORCE-CLEARS this to `false` for a
    /// detached spawn (see [`SpawnSpec::detach`]) — a stdin pipe's write end
    /// would be held by the soon-to-exit spawning CLI, so a detached child is
    /// spawned with `Stdio::null()` stdin unconditionally (no EPIPE strand);
    /// after re-adoption `send` fails with the ordinary adopted-instance
    /// interaction error, exactly as an adopted handle always has.
    pub pipe_stdin: bool,
    /// Whether this spawn is DETACHED (story 12-1) — the spawn-time mode whose
    /// handle is DISARMED so the child survives the spawning engine's exit.
    ///
    /// Detach is a SPAWN-TIME property, never a post-hoc disown (AI-20 option
    /// b): the backend builds the handle differently from the first moment. On
    /// Unix the child already runs in its own session (`setsid`, `pgid == pid`),
    /// so a detached `UnixProcess` simply SKIPS the Drop kill-on-drop (stop /
    /// pause / resume still work in-process via `killpg` on the pgid). On
    /// Windows the Job Object (whose `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is
    /// the only thing that would kill the child at handle-drop) is NOT created
    /// and the child is NOT assigned to one — kill-on-close never exists, so
    /// nothing has to be undone mid-flight (no breakaway, no Job redesign —
    /// the fracture-prone path the epic's research flagged). The
    /// write-ahead fingerprint machinery (AD-5) is UNCHANGED: a detached spawn
    /// still records its verified `{pid, start-time}` so the next engine open
    /// re-adopts the surviving child through the EXISTING fingerprint path.
    /// `false` (every pre-12-1 caller) keeps today's kill-on-drop guarantee.
    pub detach: bool,
    /// Whether to pipe the child's STDOUT to the engine instead of the
    /// crash-immune file redirect (story 14-1, spine AD-19) — the `acp`
    /// transport consumes the child's stdout as the protocol stream (ndJSON
    /// over the pipe), and the connection's reader appends each raw line
    /// back to `log_file` so the AD-12 raw record stays the honest record.
    /// `false` (every pre-14-1 caller) keeps the direct file redirect
    /// byte-for-byte. Detached acp spawns are NOT piped (no transport for a
    /// detached start) — the supervisor resolves this bool, the backends
    /// stay capability-agnostic dumb executors, exactly like
    /// [`SpawnSpec::pipe_stdin`].
    pub pipe_stdout: bool,
}

/// The outcome of a [`ProcessBackend::stop`] call (AC3).
///
/// Records whether the graceful request sufficed or the backend had to escalate
/// to a forced kill after the window, so the supervisor can record the
/// escalation in the instance log and emit the right transition cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StopOutcome {
    /// `true` if the graceful window elapsed and the process (group/job) was
    /// force-killed; `false` if it exited gracefully within the window.
    pub forced: bool,
}

/// Whether a supervised process is still alive (used for the `starting→running`
/// readiness check and `stopping→stopped` detection).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessStatus {
    /// The process is still running.
    Alive,
    /// The process has exited; carries its exit code if one was reported.
    Exited {
        /// The exit code, if the OS reported one (`None` if killed by a signal
        /// with no code, etc.).
        code: Option<i32>,
    },
}

impl ProcessStatus {
    /// Whether the process has exited.
    pub fn is_exited(&self) -> bool {
        matches!(self, ProcessStatus::Exited { .. })
    }
}

/// A durable fingerprint of a supervised process (spine AD-5) — the PID-reuse
/// guard behind orphan adoption.
///
/// AD-5 requires the write-ahead spawn record to carry the process
/// `{ pid, start-time fingerprint }` so that, after an engine crash + restart,
/// a persisted record can be reconciled against live processes WITHOUT a false
/// adoption when the OS has recycled the PID for an unrelated new process. Two
/// different processes that happen to reuse a PID have DIFFERENT start-times, so
/// comparing the recorded `start_time` to a live PID's start-time is the guard:
/// a match adopts, a mismatch (or a gone PID) reconciles to `failed`.
///
/// `start_time` is an OPAQUE, monotonic-per-boot token whose UNIT is per-OS
/// (Linux: clock ticks since boot from `/proc/<pid>/stat` field 22; macOS: the
/// process start-time from `libproc` `proc_pidinfo(PROC_PIDTBSDINFO)`
/// (`pbi_start_tvsec`/`pbi_start_tvusec`) folded to microseconds; Windows: the
/// `GetProcessTimes` creation time in 100ns ticks). Its only contract is: STABLE for a given
/// process across reads, and DIFFERENT across a PID reuse. It is a domain value
/// (no OS type crosses the port); the per-OS sources live only in `backends/`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessFingerprint {
    /// The OS process id.
    pub pid: u32,
    /// The opaque, per-OS process start-time token (see the type docs). Stable
    /// per process; differs across a PID reuse.
    pub start_time: u64,
}

impl ProcessFingerprint {
    /// Construct a fingerprint from a pid + start-time token.
    pub fn new(pid: u32, start_time: u64) -> Self {
        Self { pid, start_time }
    }

    /// Whether this fingerprint identifies the SAME process as `other`: both the
    /// pid AND the start-time token must match. A pid match with a different
    /// start-time is a PID reuse (a DIFFERENT process), not the same one.
    pub fn matches(&self, other: &ProcessFingerprint) -> bool {
        self.pid == other.pid && self.start_time == other.start_time
    }
}

/// Why a process operation failed, in domain terms (never an OS error code).
///
/// The backends map their OS failures into these variants; the supervisor maps
/// them onward into an [`EngineError`](crate::domain::EngineError). `thiserror`
/// in the engine (no miette here — conventions).
#[derive(Debug, Error)]
pub enum BackendError {
    /// The process could not be spawned (exec not found, permission denied, a
    /// bad working dir, …). Carries the exec and the underlying detail so the
    /// diagnostic can be preserved verbatim (AC2).
    #[error("could not launch '{exec}': {detail}")]
    Spawn {
        /// The executable that failed to launch.
        exec: String,
        /// The underlying OS/spawn error detail (preserved for AC2).
        detail: String,
    },

    /// The process spawned but exited immediately during startup with a failure
    /// code — treated as a launch failure (AC2). Carries the code.
    #[error("'{exec}' exited immediately during startup with code {code}")]
    ImmediateExit {
        /// The executable.
        exec: String,
        /// The non-zero exit code observed at startup.
        code: i32,
    },

    /// A stop/kill/poll/pause/resume syscall failed unexpectedly. Carries the
    /// operation and detail. (A process that is already gone is NOT an error —
    /// that is the desired end state for stop; a SIGSTOP/SIGCONT to a gone group
    /// likewise resolves to success via `ESRCH`, see the Unix backend.)
    #[error("process control operation '{op}' failed: {detail}")]
    Control {
        /// The operation that failed (`"signal"`, `"wait"`, `"terminate"`,
        /// `"pause"`, `"resume"`, `"fingerprint"`, `"adopt"`, …).
        op: &'static str,
        /// The underlying detail.
        detail: String,
    },

    /// A [`ProcessBackend::write_stdin`] call did not complete within
    /// [`STDIN_WRITE_TIMEOUT`] (story 4.1 fix pass, the CRITICAL finding —
    /// review of #79). See [`write_stdin_bounded`]'s docs for the full
    /// mechanism. The instance's interaction channel is now PERMANENTLY
    /// broken for the remainder of this engine session (a stop/start builds
    /// an entirely fresh handle/pipe).
    #[error(
        "stdin write did not complete within {timeout_secs}s — the agent may not be draining its input"
    )]
    StdinTimedOut {
        /// The bound (seconds) that elapsed before the write was abandoned.
        timeout_secs: u64,
    },

    /// A [`ProcessBackend::stop`] call sent SIGKILL (or the platform
    /// equivalent — `TerminateJobObject`/`TerminateProcess` on Windows) but
    /// could not CONFIRM the process's death within [`KILL_CONFIRM_TIMEOUT`]
    /// (fix pass, review of #80 follow-up — the CRITICAL finding: see
    /// `KILL_CONFIRM_TIMEOUT`'s docs for the full mechanism). The process is
    /// NOT necessarily gone — it may still be alive, stuck in an OS-level
    /// uninterruptible I/O wait that no signal can interrupt — so the caller
    /// must NOT treat this as a successful stop.
    #[error(
        "SIGKILL was sent but the process has not been confirmed dead within {timeout_secs}s \
         (it may be stuck in an OS-level I/O wait, e.g. disk pressure)"
    )]
    StopUnconfirmed {
        /// The bound (seconds) that elapsed before confirmation was abandoned.
        timeout_secs: u64,
    },
}

/// How long a [`ProcessBackend::write_stdin`] call may block before the
/// instance's interaction channel is declared permanently broken for the rest
/// of this engine session (story 4.1 fix pass — the CRITICAL finding: the
/// ENTIRE engine shares ONE supervisor lock — `EngineInner::supervisor` in
/// `engine.rs` — so an unbounded blocking write to one instance's stdin pipe
/// could freeze every other instance's `start`/`stop`/`pause`/`send` AND the
/// crash-detection reaper forever, since none of them can acquire the lock
/// until the write returns; an adversarial audit reproduced this empirically
/// against the story's original unbounded `write_all`).
///
/// 5 seconds is a conservative, deliberately non-configurable bound (this is
/// an internal resilience bound, not a user-facing setting — no new CLI
/// flag/config surface is warranted, since neither `epics.md` nor the story
/// calls for one): a healthy agent draining the realistic operator payload
/// (`send`'s `text` argument — a line or a short paragraph of interactive
/// input) completes in low-single-digit milliseconds even on a loaded CI
/// runner, so 5s leaves generous headroom for scheduler jitter while still
/// keeping a genuinely stuck agent from wedging the engine for anything
/// beyond a human-noticeable pause. It intentionally mirrors the order of
/// magnitude of [`crate::domain::DEFAULT_STOP_WINDOW`]'s sibling bounded-wait
/// (30s) scaled down for a much smaller, interactive-sized payload rather
/// than a whole graceful-shutdown grace period.
pub const STDIN_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long [`ProcessBackend::stop`] may block CONFIRMING a process's death
/// after SIGKILL (or the platform equivalent) has already been sent, before
/// giving up and reporting [`BackendError::StopUnconfirmed`] instead of
/// continuing to wait (fix pass, review of #80 follow-up — the CRITICAL
/// finding).
///
/// **The mechanism this bounds:** the EARLIER fix pass (review of #80) closed
/// a crash-safety bug by moving agent stdout/stderr from `Stdio::piped()`
/// (read by an engine-owned reader thread) to a DIRECT `Stdio::from(file)`
/// redirect — the right fix for that problem (a pipe's read end vanishing
/// when the engine crashes must never be able to kill, or blind capture for,
/// the supervised agent). But a pipe's finite kernel buffer was ALSO
/// providing INCIDENTAL backpressure: a slow (or, after that fix, entirely
/// absent) reader naturally throttled a fast writer. A direct file has no
/// such throttle — a child can write at raw disk I/O speed (empirically
/// ~1.1-2 GB/s), so a sufficiently fast/unbounded writer can exhaust
/// available disk space. Once that happens, the writing process can enter an
/// OS-level UNINTERRUPTIBLE I/O wait (Linux `D` state; macOS `U`/`Us` per
/// `ps aux`) — a state that does not respond to ANY signal, including
/// SIGKILL, until the underlying I/O operation resolves at the
/// kernel/storage layer. Before this bound, [`ProcessBackend::stop`] waited
/// for confirmed death with NO deadline at all in this phase (a bare,
/// unbounded `Child::wait()` on Unix/Windows) — and `stop` runs while the
/// caller holds the engine-wide `EngineInner::supervisor` lock (`engine.rs`),
/// so a single unconfirmable process could freeze EVERY other instance's
/// `start`/`stop`/`pause`/`resume`/`send` and the crash-detection reaper, for
/// an OS-determined, UNBOUNDED duration — worse than
/// [`STDIN_WRITE_TIMEOUT`]'s already-bounded residual, because this one had
/// no upper bound at all.
///
/// 5 seconds mirrors [`STDIN_WRITE_TIMEOUT`]'s existing precedent (story 4.1
/// fix pass) and the informal 5-second bound this codebase already used for
/// an ADOPTED handle's post-kill confirmation poll (which this fix pass also
/// makes honest — see the backends' `stop()` docs): a NORMAL process (i.e.
/// NOT stuck in an uninterruptible wait) is reaped within low-single-digit
/// MILLISECONDS of a SIGKILL/`TerminateJobObject`/`TerminateProcess`, so 5
/// seconds leaves generous headroom for scheduler jitter while still keeping
/// a genuinely stuck process from wedging the engine for anything beyond a
/// human-noticeable pause. Deliberately non-configurable — an internal
/// resilience bound, not a user-facing setting, mirroring
/// [`STDIN_WRITE_TIMEOUT`]'s own precedent.
///
/// This does NOT (and cannot) make an uninterruptible-wait process killable —
/// that is an OS-level limitation outside the engine's control. It bounds
/// only the ENGINE's own waiting/blocking behavior: see
/// [`crate::domain::Supervisor::stop`]'s docs for how the domain layer
/// reconciles the instance's state once this bound is hit (an honest
/// [`crate::domain::EngineError::StopUnconfirmed`], never a false `stopped`)
/// and how it avoids compounding the wait on an immediate retry.
pub const KILL_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);

/// The per-generation byte cap for the attributed output capture (story 4-2,
/// AD-12, epics.md's literal "10MB × 3"). A FIXED, non-configurable engine
/// resilience bound (no unified-config key this story) — mirrors
/// [`STDIN_WRITE_TIMEOUT`]'s precedent of a hardcoded bound rather than a
/// user setting. Checked BEFORE each append ([`should_rotate`]), so a single
/// generation may end up a little over this exact byte count (the cost of a
/// cheap check-before-write rather than a mid-line truncation); this is an
/// accepted approximation, not an exact ceiling.
pub const LOG_ROTATE_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// How many generations of the attributed capture are retained: the CURRENT
/// file plus this many minus one rotated predecessors (`.1`, `.2`, …) — AD-12's
/// "10MB × 3" (current + `.1` + `.2`). A read that spans a rotation boundary
/// honestly returns whatever these retained generations hold; `kt agent logs`
/// never errors due to rotation.
pub const LOG_ROTATE_GENERATIONS: u8 = 3;

/// Whether the attributed capture's CURRENT generation should be rotated
/// BEFORE the next append, given its current byte length (story 4-2, Task 1).
/// Pure, no I/O — unit-testable at the exact boundary. Uses `>=` (the `≥`
/// boundary convention this codebase uses everywhere else, e.g.
/// `BudgetEvaluator`): a generation that has JUST reached the cap rotates on
/// its NEXT append, rather than growing past it first.
fn should_rotate(current_len: u64) -> bool {
    current_len >= LOG_ROTATE_MAX_BYTES
}

/// The state of a spawned process's stdin channel (story 4.1 fix pass — the
/// CRITICAL and HIGH findings, review of #79).
///
/// Deliberately not a bare `Option<ChildStdin>` (as the story originally
/// shipped it): a bounded write timeout ([`STDIN_WRITE_TIMEOUT`]) introduces
/// a THIRD possibility beyond "has a pipe" / "never had one" — "had one, but
/// a write to it never came back in time, so it can never be safely touched
/// again". [`StdinState::TimedOut`] must stay clearly distinguishable from
/// both [`StdinState::NoPipe`] and [`StdinState::Live`].
#[derive(Debug)]
pub enum StdinState {
    /// No pipe is held for this handle. Covers BOTH: an ADOPTED process (no
    /// OS-portable, documented way to recover a pipe file descriptor from a
    /// bare PID; see [`ProcessBackend::adopt`]'s docs) and a FRESHLY SPAWNED
    /// process whose declared `Capability::Interaction` level was
    /// `Unsupported` on this OS, so [`SpawnSpec::pipe_stdin`] was `false` and
    /// the child's stdin was never piped at all (the HIGH finding's fix —
    /// `Stdio::null()`, the pre-story-4.1 safe default).
    NoPipe,
    /// A live pipe this handle can write to right now.
    Live(ChildStdin),
    /// A write to this pipe exceeded [`STDIN_WRITE_TIMEOUT`]. The spawned
    /// write thread (see [`write_stdin_bounded`]) may STILL be running,
    /// blocked on the OS write syscall — there is no safe, portable way to
    /// cancel a running [`std::thread`] or reclaim a `ChildStdin` it may
    /// still hold, so this state is PERMANENT for the remainder of this
    /// handle's life (until the instance is stopped and started again, which
    /// builds an entirely fresh handle/pipe). Every SUBSEQUENT `send` on this
    /// instance must fail fast on this state, never attempting another
    /// doomed write.
    TimedOut,
}

impl StdinState {
    /// Whether this state holds a live pipe that can be written to right now.
    pub fn is_live(&self) -> bool {
        matches!(self, StdinState::Live(_))
    }

    /// Whether a PRIOR write on this state exceeded [`STDIN_WRITE_TIMEOUT`]
    /// (see [`StdinState::TimedOut`]).
    pub fn is_timed_out(&self) -> bool {
        matches!(self, StdinState::TimedOut)
    }
}

/// Write `data` to `state`'s pipe and flush it, bounded to `timeout` (story
/// 4.1 fix pass — the CRITICAL finding, review of #79).
///
/// A portable, `std`-only bounded-write pattern that needs no raw
/// non-blocking I/O or fd manipulation (which would need different,
/// error-prone implementations per OS, unlike this function which is called
/// identically from both backends): TAKES OWNERSHIP of the `ChildStdin` out
/// of `state` (via [`std::mem::replace`], so nothing else can observe or
/// touch it while a write may be in-flight), moves it onto a spawned
/// [`std::thread`] that performs the actual `write_all` + `flush`, and blocks
/// the CALLER (only — never the child thread) up to `timeout` on an
/// [`mpsc::channel`]'s [`mpsc::Receiver::recv_timeout`].
///
/// Outcomes:
/// * **Success within `timeout`**: the spawned thread's `(ChildStdin,
///   io::Result<()>)` comes back on the channel. The `ChildStdin` is put BACK
///   into `state` (`Live`) so a LATER write on the same handle can reuse the
///   same pipe — safe because the thread has already finished with it (its
///   `write_all`/`flush` calls returned before sending, and ownership moved
///   back through the channel, so there is no concurrent access).
/// * **A genuine I/O failure within `timeout`** (e.g. `EPIPE` — the agent
///   exited between the caller's liveness check and this write): reported as
///   [`BackendError::Control`], exactly like every other backend op. The
///   `ChildStdin` is likewise put back (an I/O error means the thread
///   returned, not that it is stuck).
/// * **Timeout**: the thread may still be blocked on the write syscall.
///   Reclaiming the `ChildStdin` would race a write that could complete at
///   any moment on a handle nothing else should concurrently touch, so
///   `state` is set to [`StdinState::TimedOut`] and this returns
///   [`BackendError::StdinTimedOut`]. The thread is intentionally NEITHER
///   joined NOR aborted (there is no safe, portable way to cancel a blocked
///   `std::thread`); it is simply abandoned. If the write eventually
///   completes or fails, its result is sent to a receiver nobody is
///   listening to anymore (a harmless dropped `Err(SendError)`) and the
///   thread exits normally, dropping its `ChildStdin` (closing the pipe).
///   This leaks the thread until the write unblocks (the agent starts
///   draining, the pipe breaks, or the process exits) or the whole engine
///   process exits; acceptable because a stuck agent is already an
///   operator-actionable failure (the diagnostic says to restart the
///   instance), not a steady-state occurrence.
///
/// Called when `state` is NOT [`StdinState::Live`] (no live pipe: `NoPipe` or
/// already `TimedOut`) is a defensive misuse the caller must check for first
/// via [`StdinState::is_live`] / [`ProcessBackend::has_stdin`] — `state` is
/// left UNCHANGED (never fabricating a timeout on a handle that never had, or
/// already lost, a pipe) and this returns [`BackendError::Control`], mirroring
/// the pre-fix defensive-misuse contract.
pub(crate) fn write_stdin_bounded(
    state: &mut StdinState,
    data: &[u8],
    timeout: Duration,
) -> Result<(), BackendError> {
    let stdin = match std::mem::replace(state, StdinState::TimedOut) {
        StdinState::Live(stdin) => stdin,
        other => {
            // Defensive misuse: restore the ORIGINAL state unchanged.
            *state = other;
            return Err(BackendError::Control {
                op: "stdin",
                detail: "no stdin pipe held for this handle".to_string(),
            });
        }
    };
    // `state` is now provisionally `TimedOut` for the duration of the write —
    // the honest, conservative default if this function returns early (e.g. a
    // future refactor introduces an early return) without reaching the result
    // handling below: a handle must never look reusable when we are not sure
    // it is.
    let (tx, rx) = mpsc::channel();
    let payload = data.to_vec();
    thread::spawn(move || {
        let mut stdin = stdin;
        let result = stdin.write_all(&payload).and_then(|()| stdin.flush());
        // The receiver may already be gone (the caller timed out) — a send
        // error just means nobody is listening anymore; either way the
        // thread exits normally here, dropping `stdin` on the tuple's drop
        // (closing the pipe) if nobody claimed it.
        let _ = tx.send((stdin, result));
    });
    match rx.recv_timeout(timeout) {
        Ok((stdin, Ok(()))) => {
            *state = StdinState::Live(stdin);
            Ok(())
        }
        Ok((stdin, Err(e))) => {
            *state = StdinState::Live(stdin);
            Err(BackendError::Control {
                op: "stdin",
                detail: e.to_string(),
            })
        }
        Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            // Timeout, or the thread panicked before sending (defensive —
            // write_all/flush do not panic in practice). `state` is already
            // TimedOut (set above); the thread may still be running/blocked,
            // so the ChildStdin can never be safely reclaimed.
            Err(BackendError::StdinTimedOut {
                timeout_secs: timeout.as_secs(),
            })
        }
    }
}

// ---- Story 4-2 (AD-12), fix pass (review of #80): crash-immune capture ----
//
// ROOT CAUSE this fix pass closes: story 4-2 originally connected the child's
// stdout/stderr to `Stdio::piped()` unconditionally, with the ENGINE holding
// the pipes' read ends (consumed by reader threads, for attribution). That
// coupled the agent's write-SUCCESS to the engine's LIVENESS for the first
// time: if the engine process exits by any means that skips its own `Drop`
// (SIGKILL, panic-abort, OOM), the OS closes its fd table, the pipe's sole
// read-end reference vanishes, and the agent's NEXT `write()` gets `EPIPE` —
// which kills a default-SIGPIPE-disposition process outright (the common case
// for shell scripts, C programs, and many non-Rust/non-Python agent CLIs) and,
// for a SIGPIPE-immune agent (Rust/Python's default), permanently ends output
// capture for the rest of that instance's life. This is a direct regression
// against NFR-1 (an engine crash must never be able to kill, or permanently
// blind us to, a process it is supposed to be resiliently supervising).
//
// THE FIX: eliminate the pipe entirely for output capture. The child's
// stdout and stderr are each redirected DIRECTLY to their OWN regular file
// (`Stdio::from(file)`, one file per stream — see [`SpawnSpec::log_file`] /
// [`SpawnSpec::stderr_log_file`]), exactly the crash-immune mechanism this
// codebase used for `agent.log` BEFORE story 4-2 ever piped anything: a
// regular file never generates `SIGPIPE`/`EPIPE` on write regardless of
// whether anything is reading it, so the agent's own `write()` succeeds or
// fails based ONLY on that file, NEVER on the engine's liveness. The stdout
// file IS `agent.log` (CRITICAL SCOPING #3 — byte-identical raw content,
// still what `drain_usage_for` reads directly and synchronously, so its
// billing-critical sentinel read has ZERO added hop latency, same as before
// story 4-2 ever existed — this is also the H2 fix: `drain_usage_for`'s
// terminal, pre-kill read can no longer race a reader thread's own schedule,
// because there is no longer a reader thread between the agent's write() and
// this file).
//
// ATTRIBUTION + ROTATION (the actual point of story 4-2) now come from a
// background TAILER: a per-instance thread that incrementally re-reads
// (cursor-based, the same idea [`crate::domain::Supervisor::read_agent_log_since`]
// already uses) the two crash-immune raw files and re-attributes+rotates
// their content into `output.log[.N]` (via [`append_attributed_line`],
// UNCHANGED below). This is now a DERIVED, best-effort VIEW: it can lag, or
// stop entirely if the engine crashes, WITHOUT harming the agent's own
// writes at all (unlike the pre-fix reader threads, whose live pipes the
// agent's writes depended on).
//
// H1 fix (review of #80, second finding): the ORIGINAL version of this fix
// pass kept story 4-2's single-background-writer-thread-fed-by-a-channel
// design for `output.log` (only the READER side changed, from blocking pipe
// reads to a polling tailer). That left a DIFFERENT async hop in place for
// Task 4's `Engine`-attributed lines: `transition_with_log_capture` would
// `send` a line into the channel and return immediately, trusting the
// writer thread to eventually dequeue and append it — but a caller that
// starts an instance and then EXITS right away (e.g. a `kt agent start`-style
// CLI invocation, or this fix pass's own crash-adoption test harnesses) can
// race that writer thread's own `recv`/write cycle, losing the
// just-enqueued "engine: ... -> running" line entirely if the process ends
// before the writer thread is even scheduled once (confirmed empirically:
// `logs_reads_retained_output_after_the_instance_stops_unix` failed
// DETERMINISTICALLY, not just flakily, once this was the only remaining
// async hop). There is no way to "join" that thread from a caller doing
// `std::process::exit` immediately after — the ONLY robust fix is to remove
// the asynchronous hand-off entirely for this write path, not merely narrow
// its window. So `output.log`'s writes are now FULLY SYNCHRONOUS too: NO
// background writer thread, NO channel — [`LogCapture::send_engine_line`]
// and the tailer's own catch-up pass both call [`append_attributed_line`]
// DIRECTLY, on whichever thread is doing the work, serialized by ONE shared
// [`Mutex`] (guarding both the tail cursors AND the rotate-then-append
// sequence) so two callers can never interleave a rotation or corrupt the
// file — the same non-negotiable property the story's ORIGINAL single-writer-
// thread design existed to guarantee, just via a lock instead of a channel
// (a substitution the story's own Dev Notes anticipated a reviewer might
// expect: "a future reviewer might expect a `Mutex<File>` instead"). This
// makes `send_engine_line` (and thus every lifecycle transition) block
// briefly on a small, cheap disk append — a deliberate, bounded trade
// (microseconds, not the unbounded-write hazard 4.1's `STDIN_WRITE_TIMEOUT`
// fix pass guarded against) in exchange for a WRITE THAT HAS DEMONSTRABLY
// HAPPENED before the call returns, which is the only way to make this
// class of race structurally impossible rather than merely unlikely.

/// Append ONE [`LogLine`] to the attributed capture at `path` — a thin,
/// single-item wrapper around [`append_attributed_lines`] (see its docs for
/// the rotation/best-effort contract). Kept as its own name because it is
/// the direct call [`LogCapture::send_engine_line`] makes (exactly one
/// line) and because several existing unit tests already call it by this
/// name.
fn append_attributed_line(path: &Path, line: &LogLine) {
    append_attributed_lines(path, std::slice::from_ref(line));
}

/// Append a BATCH of [`LogLine`]s to the attributed capture at `path` in
/// ONE file open (fix pass, review of #80 — the performance/liveness
/// finding this batching closes): checks [`should_rotate`] ONCE against the
/// size BEFORE this batch (rotating first if needed), then opens `path`
/// ONCE and writes every line before closing — never one open-check-write-
/// close cycle PER line. This matters because [`tail_new_lines`] can hand
/// this dozens to thousands of lines from a single catch-up pass (a fast,
/// bursty writer with no backpressure — e.g. a real OS process writing
/// directly to its own regular-file redirect, exactly the crash-immune
/// mechanism this fix pass introduces — can accumulate a large backlog
/// between polls): a naive one-open-per-line loop over such a backlog would
/// make a SINGLE [`LogCapture::send_engine_line`] call (which runs
/// SYNCHRONOUSLY, potentially while the caller holds the engine-wide
/// supervisor lock — see `domain::supervisor`'s `EngineInner`) take
/// seconds to minutes purely on open/close syscall overhead, a genuine
/// engine-freezing hazard this batching eliminates (empirically confirmed
/// during this fix pass's own crash-kill experiment: an unbounded `yes`
/// process, writing with zero backpressure to its direct-file redirect,
/// produced a backlog large enough that the pre-batching implementation
/// spun for minutes and exhausted tens of gigabytes of disk before this fix
/// was applied). A single generation may end up slightly over
/// [`LOG_ROTATE_MAX_BYTES`] as a result (the whole batch lands after one
/// rotation check, never mid-batch) — an accepted approximation, consistent
/// with [`should_rotate`]'s existing "checked before, not enforced
/// mid-write" contract. `lines` is capped by
/// [`MAX_TAIL_LINES_PER_PASS`]/[`MAX_TAIL_BYTES_PER_PASS`] at the caller
/// ([`tail_new_lines`]), so a single batch here is itself bounded.
///
/// A free function (not a method) so it is directly unit-testable with no
/// thread synchronization needed. Best-effort: a write hiccup here must
/// never crash the engine over a captured-log line, mirroring this
/// codebase's existing best-effort discipline for background capture (e.g.
/// `drain_usage_for`'s read-failure skip).
fn append_attributed_lines(path: &Path, lines: &[LogLine]) {
    if lines.is_empty() {
        return;
    }
    let current_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if should_rotate(current_len) {
        rotate_generations(path, LOG_ROTATE_GENERATIONS);
    }
    // Serialize the WHOLE batch into ONE in-memory buffer and issue ONE
    // `write_all` call (fix pass, review of #80 — a second, independent
    // performance finding from the same crash-kill experiment this batch
    // API was introduced for): a naive per-line `writeln!` on a bare,
    // unbuffered `File` is a SEPARATE `write()` SYSCALL per line — for a
    // batch of hundreds of thousands of tiny lines (a fast, bursty writer
    // with no backpressure can accumulate exactly that many within
    // [`MAX_TAIL_BYTES_PER_PASS`]), that is still hundreds of thousands of
    // syscalls despite the file being opened only once, which empirically
    // took SECONDS — long enough to matter for a call
    // ([`LogCapture::send_engine_line`]) that may run while the engine-wide
    // supervisor lock is held. Building one buffer and writing it in a
    // single call reduces this to O(1) syscalls regardless of batch size.
    let mut buf = String::with_capacity(lines.len() * 96);
    for line in lines {
        let Ok(json) = serde_json::to_string(line) else {
            continue;
        };
        buf.push_str(&json);
        buf.push('\n');
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(buf.as_bytes());
    }
}

/// The rotated-generation sibling of `base` (story 4-2) — `base` with `.N`
/// appended to its raw [`std::ffi::OsStr`] (not a lossy `Display`
/// concatenation, so a non-UTF8 path stays exact). `n == 1` is the most-
/// recently-rotated generation; higher `n` is older. Mirrors
/// [`crate::domain::Registry::attributed_output_log_generation_path`]'s
/// naming exactly (`output.log.<n>`) so the read side and the write side
/// agree on every generation's path without either depending on the other.
fn generation_path(base: &Path, n: u8) -> PathBuf {
    let mut os = base.as_os_str().to_os_string();
    os.push(format!(".{n}"));
    PathBuf::from(os)
}

/// Rotate `base`'s generations (story 4-2, Task 2): discard the oldest
/// retained generation, shift every remaining generation up by one, then
/// rename `base` itself (the current generation) to `.1`. The caller opens a
/// FRESH `base` on its next append (this function never creates one) — an
/// open `File` handle held across a rename keeps writing to the RENAMED
/// inode, not the new path, which is exactly why [`append_attributed_line`]
/// reopens the path fresh on every call rather than holding a handle.
///
/// `generations` is [`LOG_ROTATE_GENERATIONS`] in production (current, `.1`,
/// and `.2`); a missing source file/generation is a silent no-op (`rename`
/// and `remove_file` errors are ignored) — rotation is best-effort
/// resilience, never a hard failure.
fn rotate_generations(base: &Path, generations: u8) {
    if generations <= 1 {
        // No history retained: defensive-only in production (the constant is
        // fixed at 3), but stay correct — just drop the current generation.
        let _ = std::fs::remove_file(base);
        return;
    }
    let oldest = generations - 1;
    let _ = std::fs::remove_file(generation_path(base, oldest));
    for gen in (1..oldest).rev() {
        let _ = std::fs::rename(generation_path(base, gen), generation_path(base, gen + 1));
    }
    let _ = std::fs::rename(base, generation_path(base, 1));
}

/// How often the background tailer thread ([`spawn_tailer_thread`]) re-reads
/// the crash-immune raw per-stream files for new, complete lines (fix pass,
/// review of #80) — mirrors [`crate::backends::unix::STOP_POLL_INTERVAL`]'s
/// existing precedent of a short, hardcoded poll bound. Fast enough that
/// `output.log`'s attributed view stays close to real time (well within the
/// generous multi-second deadlines every existing follow/attribution test
/// already tolerates), cheap enough (two small file reads per tick) not to
/// be wasteful.
const LOG_TAIL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The MAXIMUM number of new bytes [`tail_new_lines`] reads (and thus the
/// most it ever hands to ONE [`append_attributed_lines`] batch) per call
/// (fix pass, review of #80). This is the bound that keeps
/// [`LogCapture::send_engine_line`] — which runs SYNCHRONOUSLY and may be
/// called while the engine-wide supervisor lock is held — from blocking for
/// an unbounded time when a fast, bursty writer (a real OS process with no
/// backpressure at all, since it writes to a direct file redirect, never a
/// pipe) has accumulated a large backlog between polls. A backlog LARGER
/// than this bound is simply left for the NEXT catch-up pass (the tailer's
/// own next tick, or the next `send_engine_line`/tailer call to acquire the
/// lock) — never fully "lost", just spread across more passes. 4MB keeps a
/// single pass fast (well under [`LOG_ROTATE_MAX_BYTES`], so it can never
/// itself force more than one rotation) while still making rapid progress
/// against a genuinely large backlog.
const MAX_TAIL_BYTES_PER_PASS: u64 = 4 * 1024 * 1024;

/// The MAXIMUM number of LINES [`tail_new_lines`] processes per call (fix
/// pass, review of #80 — a SECOND bound, alongside
/// [`MAX_TAIL_BYTES_PER_PASS`], found necessary by this fix pass's own
/// empirical crash-kill experiment). [`MAX_TAIL_BYTES_PER_PASS`] alone does
/// NOT bound the per-call WORK for a writer that emits many TINY lines (a
/// real OS process — e.g. `yes`, or any tight `while true; do echo ...; done`
/// loop — writing millions of short lines per second): even after batching
/// every append into ONE file write ([`append_attributed_lines`]), each
/// line still needs its OWN [`LogLine`] construction and
/// `serde_json::to_string` call — empirically, ~2 MILLION tiny lines
/// (a 4MB batch of 2-byte "y\n" lines) took ~1s even in an OPTIMIZED
/// release build, and ~15s in an unoptimized debug build (the build this
/// project's own test suite and gates run under) — far too slow for a call
/// that may run while the engine-wide supervisor lock is held.
/// Capping the LINE COUNT (not just the byte count) directly bounds this
/// per-line work regardless of how tiny individual lines are; a backlog
/// with many more lines than this is simply spread across more passes,
/// exactly like exceeding the byte bound.
const MAX_TAIL_LINES_PER_PASS: usize = 2000;

/// How far into each of the two crash-immune raw per-stream files the
/// tailer has already folded content into the attributed capture (fix
/// pass). This ONE [`Mutex`] is the SOLE synchronization primitive for
/// `output.log`: it guards both the cursor state AND every
/// rotate-then-append sequence (via [`LogCapture::catch_up_locked`]),
/// serializing the background tailer thread against any INLINE caller
/// ([`LogCapture::send_engine_line`]) so the two can never interleave a
/// rotation or a write (H1 fix — see the module docs above).
#[derive(Default, Debug)]
struct TailCursors {
    /// Bytes of [`LogCapture::stdout_raw`] already tailed.
    stdout: u64,
    /// Bytes of [`LogCapture::stderr_raw`] already tailed.
    stderr: u64,
}

/// A handle to one instance's output-capture pipeline (fix pass, review of
/// #80). Cloning is cheap (two `Arc` clones + small `PathBuf`/`String`
/// clones): the supervisor clones it to send `Engine`-attributed lines
/// (Task 4) via [`LogCapture::send_engine_line`].
#[derive(Clone, Debug)]
pub struct LogCapture {
    /// The Agent Instance name, stamped on every [`LogLine`] this capture
    /// produces.
    instance: String,
    /// The crash-immune, direct-redirect STDOUT file (== [`SpawnSpec::log_file`]
    /// == `agent.log`).
    stdout_raw: PathBuf,
    /// The crash-immune, direct-redirect STDERR file ([`SpawnSpec::stderr_log_file`]).
    stderr_raw: PathBuf,
    /// The attributed, rotated capture's CURRENT-generation path
    /// ([`SpawnSpec::attributed_log_path`]) — [`append_attributed_line`]'s
    /// target for every line this capture writes, directly and
    /// synchronously (H1 fix — no background writer thread, no channel).
    attributed_log_path: PathBuf,
    /// Shared, mutex-guarded tail cursors AND the write lock (see
    /// [`TailCursors`]'s docs).
    cursors: Arc<Mutex<TailCursors>>,
    /// Set by the process handle's `Drop` impl so the background tailer
    /// thread performs one final catch-up pass and exits, rather than
    /// leaking a thread per stop/restart for the remainder of the engine's
    /// life.
    stop: Arc<AtomicBool>,
}

impl LogCapture {
    /// Fold any newly-written, COMPLETE lines from both raw files DIRECTLY
    /// into the attributed capture (synchronous — [`append_attributed_line`],
    /// no channel), while `cursors` is already locked by the caller. A
    /// trailing partial line (no `\n` yet) is left for the next pass —
    /// mirrors [`crate::domain::supervisor`]'s `plan_follow`'s "only
    /// complete lines" rule.
    fn catch_up_locked(&self, cursors: &mut TailCursors) {
        tail_new_lines(
            &self.stdout_raw,
            &mut cursors.stdout,
            LogStream::AgentOut,
            &self.instance,
            &self.attributed_log_path,
        );
        tail_new_lines(
            &self.stderr_raw,
            &mut cursors.stderr,
            LogStream::AgentErr,
            &self.instance,
            &self.attributed_log_path,
        );
    }

    /// Lock `cursors` and fold in any newly-written content. Idempotent and
    /// safe to call concurrently with the background tailer thread or
    /// another [`LogCapture::send_engine_line`] call (the mutex serializes
    /// every caller); a redundant call (nothing new since the last one) is
    /// a cheap no-op.
    fn catch_up(&self) {
        let mut cursors = self
            .cursors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.catch_up_locked(&mut cursors);
    }

    /// Catch up any pending agent output, THEN append an `Engine`-attributed
    /// [`LogLine`] (story 4-2, Task 4; fix pass H1, review of #80) —
    /// SYNCHRONOUSLY, both under the SAME lock acquisition, so no
    /// concurrent tailer pass can land between the catch-up and this
    /// engine line. By the time this call RETURNS, the line is durably on
    /// disk — there is no background thread left to race a caller that
    /// exits (or crashes) immediately after (the exact race H1 identified:
    /// a helper process starting an instance then exiting right away used
    /// to lose the "engine: ... -> running" line if the old writer thread
    /// never got scheduled first).
    pub(crate) fn send_engine_line(&self, line: LogLine) {
        let mut cursors = self
            .cursors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.catch_up_locked(&mut cursors);
        append_attributed_line(&self.attributed_log_path, &line);
    }

    /// Signal the background tailer thread to perform one final catch-up
    /// pass and exit (fix pass, review of #80). Called from the process
    /// handle's `Drop` impl so a tailer thread never outlives its instance.
    pub(crate) fn signal_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Read new, COMPLETE lines from `path` since `*cursor` (a byte offset),
/// appending them as a SINGLE BATCH (synchronously —
/// [`append_attributed_lines`], one file open for the whole batch) of
/// attributed, timestamped [`LogLine`]s into `attributed_log_path` (fix
/// pass, review of #80) — the pull-based replacement for the old blocking
/// pipe reader threads, with NO channel hand-off. A trailing partial line
/// (no `\n` yet) is left un-consumed for the next call. Best-effort: any
/// read hiccup (the file momentarily missing, a transient I/O error) is a
/// silent skip, never fatal — this is a derived, best-effort VIEW, mirroring
/// this codebase's existing best-effort discipline for background capture.
///
/// BOUNDED per call to [`MAX_TAIL_BYTES_PER_PASS`] AND [`MAX_TAIL_LINES_PER_PASS`]
/// (fix pass, review of #80 — see their docs for why BOTH are needed: bytes
/// alone does not bound the per-line JSON-serialization work for a writer
/// emitting many TINY lines): a backlog larger than either bound is read/
/// attributed only up to whichever limit is hit FIRST this pass, leaving the
/// rest for the next call — this is what keeps a single pass (and thus a
/// single, synchronous [`LogCapture::send_engine_line`] call) fast
/// regardless of how large a backlog has accumulated, or how it is shaped.
///
/// A shrink (the file is now SHORTER than `*cursor`) is treated like
/// [`crate::domain::supervisor`]'s `plan_follow`'s shrink guard: snap the
/// cursor to the new length and read nothing this pass, rather than
/// re-reading from the start (which would re-attribute already-seen bytes).
/// This should not normally happen for these append-only raw captures; it is
/// a defensive guard, not an expected path.
fn tail_new_lines(
    path: &Path,
    cursor: &mut u64,
    stream: LogStream,
    instance: &str,
    attributed_log_path: &Path,
) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return;
    };
    if len <= *cursor {
        if len < *cursor {
            *cursor = len; // shrink guard — never re-read from the start.
        }
        return;
    }
    if file.seek(SeekFrom::Start(*cursor)).is_err() {
        return;
    }
    // Cap this pass's read to MAX_TAIL_BYTES_PER_PASS — a large backlog is
    // handled across MULTIPLE passes, never in one unbounded read.
    let want = (len - *cursor).min(MAX_TAIL_BYTES_PER_PASS);
    let mut buf = vec![0u8; want as usize];
    if file.read_exact(&mut buf).is_err() {
        return;
    }
    // Only whole, COMPLETE lines: up to the last '\n' WITHIN this bounded
    // window, further capped to at most MAX_TAIL_LINES_PER_PASS lines (the
    // position of the Nth newline, if the window holds more than that many)
    // — whichever limit is hit first. A trailing partial line, or any line
    // beyond the line-count cap, waits for the next pass. Splitting exactly
    // at a '\n' byte is always a safe UTF-8 boundary (a newline byte never
    // appears inside a multi-byte UTF-8 sequence), so the lossy decode
    // below never corrupts a split multi-byte character.
    let consumable = buf
        .iter()
        .enumerate()
        .filter(|(_, &b)| b == b'\n')
        .map(|(pos, _)| pos + 1)
        .take(MAX_TAIL_LINES_PER_PASS)
        .last()
        .unwrap_or(0);
    if consumable == 0 {
        return;
    }
    let text = String::from_utf8_lossy(&buf[..consumable]).into_owned();
    let at = now_rfc3339();
    let lines: Vec<LogLine> = text
        .lines()
        .map(|line| LogLine::new(instance, stream, line, at.clone()))
        .collect();
    append_attributed_lines(attributed_log_path, &lines);
    *cursor += consumable as u64;
}

/// Spawn the per-instance background tailer thread (fix pass, review of
/// #80) — the pull-based replacement for the old two blocking pipe-reader
/// threads. Loops [`LOG_TAIL_POLL_INTERVAL`], calling
/// [`LogCapture::catch_up`] on its OWN clone of `capture` — the SAME method
/// (and the SAME `cursors` mutex) [`LogCapture::send_engine_line`]'s inline
/// catch-up uses, so the two can never race each other into a
/// double-append or a torn rotation. Exits once `capture`'s `stop` flag is
/// observed set, after ONE final catch-up pass (so nothing the process
/// wrote right before being killed is stranded). Returns the
/// [`thread::JoinHandle`] so tests can `join` it deterministically after
/// signaling `stop`; production callers ([`spawn_output_capture`])
/// intentionally discard it (fire-and-forget) — the thread's lifetime is
/// bounded by `stop`, not by anyone joining it.
fn spawn_tailer_thread(capture: LogCapture) -> thread::JoinHandle<()> {
    thread::spawn(move || loop {
        let should_stop = capture.stop.load(Ordering::Relaxed);
        capture.catch_up();
        if should_stop {
            break;
        }
        thread::sleep(LOG_TAIL_POLL_INTERVAL);
    })
}

/// Wire the shared output-capture primitive into a freshly spawned process
/// (story 4-2, Task 2/3; fix pass, review of #80): starts ONE background
/// tailer thread (pull-based, reading the two crash-immune raw files —
/// [`spawn_tailer_thread`]) and returns a [`LogCapture`] the caller (a
/// backend's `spawn()`) stores on the process handle — the supervisor later
/// clones it to send `Engine`-attributed lines (Task 4) via
/// [`LogCapture::send_engine_line`], which writes SYNCHRONOUSLY (H1 fix —
/// no writer thread, no channel, for either the tailer's own catch-up or an
/// engine line).
///
/// `stdout_raw_path`/`stderr_raw_path` are the ALREADY-OPEN direct redirect
/// targets ([`SpawnSpec::log_file`]/[`SpawnSpec::stderr_log_file`]) — this
/// function takes only their PATHS (never a live pipe/fd), since it reads
/// them the same way any later reader would (a `File::open` by path), which
/// is exactly what makes this mechanism crash-immune: nothing here depends
/// on any handle the spawning engine session holds.
///
/// The tailer's cursors start at the CURRENT length of each raw file (read
/// here, before this new process can have written a single byte) rather than
/// `0` — a stop→start reuses the SAME append-only raw files (mirrors
/// `Supervisor::agent_log_len`'s identical "anchor at the pre-spawn length"
/// reasoning for the usage-ingestion cursor), so starting at `0` would
/// re-tail an entire prior Run's history into `output.log` again.
///
/// Called identically from both backends (mirrors `write_stdin_bounded`'s
/// "ONE shared implementation called identically from both `backends/unix`
/// and `backends/windows`" precedent). An ADOPTED handle never calls this —
/// there is no OS-portable way to recover which raw files a bare PID was
/// writing to independent of the registry path authority already computing
/// them, and more fundamentally no live tailer thread survives the engine
/// process that spawned it, so an adopted handle gets no [`LogCapture`] at
/// all (mirrors `stdin`'s `None`-on-adoption precedent); this is not a
/// functional gap for reading/following (AC-H), since reading only ever
/// needs the FILE these threads keep writing to, never a live handle.
pub(crate) fn spawn_output_capture(
    stdout_raw_path: PathBuf,
    stderr_raw_path: PathBuf,
    attributed_log_path: PathBuf,
    instance: String,
) -> LogCapture {
    let cursors = Arc::new(Mutex::new(TailCursors {
        stdout: std::fs::metadata(&stdout_raw_path)
            .map(|m| m.len())
            .unwrap_or(0),
        stderr: std::fs::metadata(&stderr_raw_path)
            .map(|m| m.len())
            .unwrap_or(0),
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let capture = LogCapture {
        instance,
        stdout_raw: stdout_raw_path,
        stderr_raw: stderr_raw_path,
        attributed_log_path,
        cursors,
        stop,
    };
    let _tailer_handle = spawn_tailer_thread(capture.clone());
    capture
}

/// The process-control port (spine AD-1 side port; AD-4 per-OS).
///
/// The domain/supervisor names this trait and gets a concrete implementation via
/// [`crate::backends::current`]; it never names a concrete backend or any OS
/// type. Implementors live only under `backends/{unix,windows}/`.
pub trait ProcessBackend {
    /// The backend-owned handle to a running process (the child + its
    /// group/job). Opaque to the supervisor, which stores it via a cfg-selected
    /// alias.
    type Handle: Send;

    /// Spawn the process described by `spec` into its own isolation group
    /// (a dedicated process group on Unix; a Job Object on Windows), so a later
    /// [`ProcessBackend::stop`] can terminate the WHOLE tree — catching any child
    /// processes the agent itself spawned (AC3 "no process of the instance
    /// survives"). Returns a handle on success; on a launch failure returns a
    /// [`BackendError`] and leaves NO zombie (AC2).
    fn spawn(&self, spec: &SpawnSpec) -> Result<Self::Handle, BackendError>;

    /// Request graceful shutdown of the process (group/job) and, if it has not
    /// exited within `graceful_window`, escalate to a forced kill of the whole
    /// group/job. Reaps the process so no zombie remains. Returns a
    /// [`StopOutcome`] recording whether escalation happened (AC3).
    ///
    /// **Bounded death confirmation (fix pass, review of #80 follow-up — the
    /// CRITICAL finding):** after escalating, this CONFIRMS the process is
    /// actually gone, bounded to [`KILL_CONFIRM_TIMEOUT`] — see its docs for
    /// the full mechanism (a fast writer can exhaust disk and enter an
    /// OS-level uninterruptible I/O wait immune to SIGKILL). If death is not
    /// confirmed within that bound, this returns
    /// [`BackendError::StopUnconfirmed`] rather than continuing to block —
    /// `stop` runs while the caller holds the engine-wide supervisor lock, so
    /// an unbounded wait here would freeze every other instance's
    /// `start`/`stop`/`pause`/`resume`/`send` and the crash-detection reaper.
    /// This does NOT (and cannot) make the process killable sooner — that is
    /// an OS-level limitation outside the engine's control; it only bounds
    /// the ENGINE's own waiting.
    fn stop(
        &self,
        handle: &mut Self::Handle,
        graceful_window: Duration,
    ) -> Result<StopOutcome, BackendError>;

    /// Non-blocking liveness check for the process, reaping it if it has exited
    /// (used for the `starting→running` readiness check and, later,
    /// crash detection). Never blocks.
    fn poll(&self, handle: &mut Self::Handle) -> Result<ProcessStatus, BackendError>;

    /// Suspend the process (group/job) — the GUARANTEED-path pause primitive
    /// (story 1-5, AC1). On Unix this delivers `SIGSTOP` to the whole process
    /// group, an uncatchable, verifiable suspension. On Windows there is no clean
    /// guaranteed whole-process suspend from `std` (AD-4), so the Windows body is
    /// the cooperative best-effort form (it succeeds without a hard suspension —
    /// the VISIBLE best-effort qualifier emitted by the supervisor/CLI, not the
    /// backend, is what keeps that honest).
    ///
    /// IMPORTANT — this method is only invoked by the supervisor on the
    /// GUARANTEED dispatch path (pause `SupportLevel::Guaranteed` on the current
    /// OS). The best-effort and unsupported levels never reach a backend call:
    /// the three-level DISPATCH is the supervisor's job, keyed on the declared
    /// per-OS `SupportLevel`, not the backend's. Sync (called via
    /// `spawn_blocking`, like `stop`/`spawn`/`poll`). Domain terms only.
    ///
    /// A process that has already exited is not an error (parity with `stop`):
    /// the desired suspended-or-gone end state already holds. Reports
    /// [`BackendError::Control`] (op `"pause"`/`"signal"`) only on an unexpected
    /// syscall failure.
    fn pause(&self, handle: &mut Self::Handle) -> Result<(), BackendError>;

    /// Resume the process (group/job) — the GUARANTEED-path resume primitive
    /// (story 1-5, AC1). On Unix this delivers `SIGCONT` to the whole process
    /// group; on Windows it is the cooperative best-effort counterpart of
    /// [`ProcessBackend::pause`]. Same dispatch contract, sync semantics, and
    /// already-exited tolerance as [`ProcessBackend::pause`]. Reports
    /// [`BackendError::Control`] (op `"resume"`/`"signal"`) only on an unexpected
    /// syscall failure.
    fn resume(&self, handle: &mut Self::Handle) -> Result<(), BackendError>;

    /// The OS process id of the spawned child (for the [`ProcessFingerprint`] /
    /// diagnostics). A stable accessor so the supervisor can log the pid without
    /// naming an OS type.
    fn pid(&self, handle: &Self::Handle) -> u32;

    /// The durable [`ProcessFingerprint`] of a supervised process (spine AD-5) —
    /// its `{ pid, start-time }`. Written into the write-ahead spawn record
    /// BEFORE the instance is treated as supervised, so a later engine restart
    /// can adopt it back without a PID-reuse false match.
    ///
    /// The start-time source is per-OS and lives only in `backends/`: Linux reads
    /// `/proc/<pid>/stat` field 22; macOS reads the process start-time via
    /// `libproc` `proc_pidinfo(PROC_PIDTBSDINFO)`; Windows reads the
    /// `GetProcessTimes` creation time. Reading the start-time of a process THIS backend spawned and holds
    /// alive cannot fail in normal operation; a read failure falls back to a
    /// start-time of `0` (the record still carries the pid — a degraded but
    /// honest fingerprint) rather than erroring the spawn. Sync (called via
    /// `spawn_blocking`). Domain terms only.
    fn fingerprint(&self, handle: &Self::Handle) -> ProcessFingerprint;

    /// Try to RE-ACQUIRE a live process matching `fingerprint`, for orphan
    /// adoption on engine start (spine AD-5, AC-B). Returns:
    /// * `Ok(Some(handle))` — a live process with pid `fingerprint.pid` exists
    ///   AND its current start-time equals `fingerprint.start_time` (the SAME
    ///   process, re-held so `stop`/`pause`/`poll` work on it again); the state
    ///   stays as persisted (`running`/`paused`).
    /// * `Ok(None)` — no live process matches (the pid is gone, OR it was reused
    ///   by a DIFFERENT process whose start-time differs — the PID-reuse guard);
    ///   the caller reconciles the record to `failed`.
    ///
    /// `detached` is the spawn record's detach flag (story 12-1 AMENDMENT,
    /// review loop 1): detached-ness is a CROSS-LIFETIME property, so an adopted
    /// handle for a detached spawn must be re-held DISARMED — Unix: the
    /// handle's Drop skips the process-group kill, so the engine's clean exit
    /// leaves the agent alive; Windows: an adopted handle never kills at drop
    /// (it only releases its handles), which IS the disarm. `false` keeps the
    /// story 1-6 semantics exactly: the adopting engine owns what it re-holds
    /// and tears it down at its own clean exit.
    ///
    /// Re-acquisition keeps the OS specifics (how to re-open a live PID, verify
    /// its start-time, and re-establish group/job control) inside `backends/`.
    /// An adopted handle must remain signal-able / stoppable as its original
    /// group/job (Unix: the pgid == pid is re-derived; Windows: re-open by pid).
    /// Sync (called via `spawn_blocking`). Reports [`BackendError::Control`]
    /// (op `"adopt"`) only on an unexpected syscall failure — a plain "no live
    /// match" is `Ok(None)`, not an error.
    fn adopt(
        &self,
        fingerprint: &ProcessFingerprint,
        detached: bool,
    ) -> Result<Option<Self::Handle>, BackendError>;

    /// Whether this handle holds a live stdin pipe it can write to right now
    /// (story 4.1, AC-D). `true` only for a FRESHLY SPAWNED handle whose
    /// `ChildStdin` was captured at spawn time AND whose declared
    /// `Capability::Interaction` level was `Guaranteed`/`BestEffort` on this
    /// OS (see [`SpawnSpec::pipe_stdin`], story 4.1 fix pass HIGH finding);
    /// `false` for an ADOPTED handle (no OS-portable way to recover a pipe fd
    /// from a bare `{pid, start-time}` fingerprint — the same "no
    /// undocumented API" convention already established for Windows pause),
    /// a handle that was never piped (interaction `Unsupported`), OR a handle
    /// whose write previously timed out (see
    /// [`ProcessBackend::stdin_timed_out`] to distinguish that case). A
    /// cheap, no-I/O accessor, mirroring [`ProcessBackend::pid`]'s style.
    /// Callers MUST check this before calling [`ProcessBackend::write_stdin`].
    fn has_stdin(&self, handle: &Self::Handle) -> bool;

    /// Whether a PRIOR [`ProcessBackend::write_stdin`] call on this handle
    /// exceeded the bounded timeout (story 4.1 fix pass, the CRITICAL
    /// finding, review of #79) — see
    /// [`StdinState::TimedOut`](crate::ports::StdinState::TimedOut). Distinct
    /// from [`ProcessBackend::has_stdin`] returning `false` for an ADOPTED or
    /// never-piped handle (which never had a pipe at all): this means "had a
    /// live pipe, attempted a write, and it never came back in time". A
    /// cheap, no-I/O accessor so a caller can short-circuit a doomed repeat
    /// write without attempting it. Once `true`, stays `true` for the
    /// remainder of this handle's life (a stop/start builds an entirely
    /// fresh handle).
    fn stdin_timed_out(&self, handle: &Self::Handle) -> bool;

    /// Write `data` to the process's stdin pipe and flush it — the v1
    /// interaction channel (spine AD-12) — bounded to
    /// [`STDIN_WRITE_TIMEOUT`](crate::ports::STDIN_WRITE_TIMEOUT) (story 4.1
    /// fix pass, the CRITICAL finding, review of #79: the ORIGINAL
    /// unbounded `write_all` could freeze the entire engine forever, since
    /// every instance shares one supervisor lock — see
    /// [`write_stdin_bounded`](crate::ports::write_stdin_bounded)'s docs for
    /// the full mechanism and its timeout/success/error outcomes). Callers
    /// MUST check [`ProcessBackend::has_stdin`] first: called with no live
    /// pipe, this is a defensive [`BackendError::Control`] naming the
    /// situation, not a normal path — and MUST check
    /// [`ProcessBackend::stdin_timed_out`] first too, to avoid attempting
    /// another doomed write on an already-broken channel (this method itself
    /// also defensively refuses in that case, as a safety net). A genuine
    /// OS-level write failure within the bound (e.g. the agent exited
    /// between the caller's state check and this write — `EPIPE`) maps to
    /// [`BackendError::Control`], exactly like every other backend op; a
    /// write that does not return within the bound maps to
    /// [`BackendError::StdinTimedOut`]. Sync (called via `spawn_blocking`,
    /// like every other method on this trait) — the bounded wait itself
    /// happens on the calling (blocking-pool) thread, mirroring how
    /// [`ProcessBackend::stop`]'s graceful-window wait already blocks its
    /// caller for a bounded duration.
    fn write_stdin(&self, handle: &mut Self::Handle, data: &[u8]) -> Result<(), BackendError>;

    /// A clone of this handle's output-capture pipeline, if it has one
    /// (story 4-2, AD-12; fix pass, review of #80) — `Some` for a FRESHLY
    /// SPAWNED handle (capture is unconditional and capability-independent,
    /// AC-E: every spawn calls [`spawn_output_capture`] whenever it has
    /// somewhere to write, independent of any declared `Capability`);
    /// `None` for an ADOPTED handle (no tailer thread was ever started for
    /// it — see [`ProcessBackend::adopt`]'s docs). A cheap accessor (a
    /// [`LogCapture`] clone is two `Arc` clones plus small `PathBuf`/`String`
    /// clones, no I/O) mirroring [`ProcessBackend::has_stdin`]'s style; used
    /// by the supervisor to send `Engine`-attributed lines (Task 4) via
    /// [`LogCapture::send_engine_line`], which writes SYNCHRONOUSLY.
    fn log_capture(&self, handle: &Self::Handle) -> Option<LogCapture>;

    /// Move the handle's stdin write half OUT (story 14-1, spine AD-19) —
    /// the `acp` transport takes exclusive ownership of the child's stdin so
    /// every ACP write (prompt, cancellation, permission denial) serializes
    /// through the connection's ONE bounded-writer mutex instead of the
    /// handle map. `Some` only for a freshly spawned handle whose stdin is
    /// still `Live` and not yet taken; `None` for an adopted handle, a
    /// never-piped one, or after a prior take (the state becomes `NoPipe` —
    /// the handle's own `has_stdin` honestly reads `false` afterwards).
    /// Cheap: an enum swap, no I/O.
    fn take_stdin(&self, handle: &mut Self::Handle) -> Option<StdinState> {
        let _ = handle;
        None
    }

    /// Move the handle's stdout read half OUT (story 14-1, spine AD-19) —
    /// the `acp` transport consumes the child's stdout as the protocol
    /// stream (the connection's reader thread appends each raw line back to
    /// the instance's `agent.log`, keeping the AD-12 raw record honest).
    /// `Some` only for a freshly spawned handle spawned with
    /// [`SpawnSpec::pipe_stdout`] and not yet taken; `None` otherwise (an
    /// adopted handle, a file-redirected stdout, or after a prior take).
    /// Cheap: an `Option::take`, no I/O.
    fn take_stdout(&self, handle: &mut Self::Handle) -> Option<std::process::ChildStdout> {
        let _ = handle;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_status_is_exited_predicate() {
        assert!(!ProcessStatus::Alive.is_exited());
        assert!(ProcessStatus::Exited { code: Some(0) }.is_exited());
        assert!(ProcessStatus::Exited { code: None }.is_exited());
    }

    #[test]
    fn stop_outcome_records_forced_flag() {
        assert!(StopOutcome { forced: true }.forced);
        assert!(!StopOutcome { forced: false }.forced);
    }

    #[test]
    fn fingerprint_matches_requires_both_pid_and_start_time() {
        // AD-5 PID-reuse guard: the SAME process matches on both fields; a pid
        // match with a different start-time is a REUSE (different process).
        let a = ProcessFingerprint::new(1234, 999);
        assert!(
            a.matches(&ProcessFingerprint::new(1234, 999)),
            "same → match"
        );
        assert!(
            !a.matches(&ProcessFingerprint::new(1234, 1000)),
            "pid reuse (start-time differs) → no match"
        );
        assert!(
            !a.matches(&ProcessFingerprint::new(5678, 999)),
            "different pid → no match"
        );
    }

    #[test]
    fn backend_error_messages_name_the_exec_and_detail() {
        let e = BackendError::Spawn {
            exec: "no-such-bin".to_string(),
            detail: "No such file or directory (os error 2)".to_string(),
        };
        let msg = e.to_string();
        assert!(msg.contains("no-such-bin"), "{msg}");
        assert!(msg.contains("No such file"), "{msg}");

        let e2 = BackendError::ImmediateExit {
            exec: "flaky".to_string(),
            code: 7,
        };
        assert!(e2.to_string().contains("code 7"));
    }

    #[test]
    fn spawn_spec_equality_is_structural() {
        let a = SpawnSpec {
            exec: "x".to_string(),
            args: vec!["--serve".to_string()],
            env: BTreeMap::new(),
            working_dir: PathBuf::from("/home"),
            log_file: None,
            attributed_log_path: None,
            stderr_log_file: None,
            instance_name: "x".to_string(),
            pipe_stdin: true,
            pipe_stdout: false,
            detach: false,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn stdin_timed_out_error_message_names_the_bound() {
        let e = BackendError::StdinTimedOut { timeout_secs: 5 };
        let msg = e.to_string();
        assert!(msg.contains('5'), "{msg}");
        assert!(msg.contains("draining"), "{msg}");
    }

    #[test]
    fn stop_unconfirmed_error_message_names_the_bound_and_the_likely_cause() {
        // Fix pass (review of #80 follow-up): the honest diagnostic must name
        // the bound and hint at the likely cause (an OS-level I/O wait), never
        // implying the process is simply gone.
        let e = BackendError::StopUnconfirmed { timeout_secs: 5 };
        let msg = e.to_string();
        assert!(msg.contains('5'), "{msg}");
        assert!(msg.contains("SIGKILL"), "{msg}");
        assert!(
            msg.contains("I/O wait"),
            "must hint at the likely cause: {msg}"
        );
    }

    #[test]
    fn kill_confirm_timeout_mirrors_the_stdin_write_timeout_precedent() {
        // Both are deliberately non-configurable 5s resilience bounds
        // (STDIN_WRITE_TIMEOUT is 4.1's precedent; KILL_CONFIRM_TIMEOUT is
        // this fix pass's own, applied to a different phase of stop()).
        assert_eq!(KILL_CONFIRM_TIMEOUT, Duration::from_secs(5));
        assert_eq!(KILL_CONFIRM_TIMEOUT, STDIN_WRITE_TIMEOUT);
    }

    #[test]
    fn stdin_state_is_live_and_is_timed_out_agree_with_the_variant() {
        // A pure-logic sanity check on the three-state predicate pair (no
        // process needed) — the process-backed proofs of the ACTUAL bounded
        // write mechanism (write_stdin_bounded's timeout/success/misuse
        // outcomes) live in backends/unix/mod.rs's test module, which has the
        // real ChildStdin-spawning infrastructure this cfg-free module does
        // not.
        assert!(!StdinState::NoPipe.is_live());
        assert!(!StdinState::NoPipe.is_timed_out());
        assert!(!StdinState::TimedOut.is_live());
        assert!(StdinState::TimedOut.is_timed_out());
    }

    #[test]
    fn write_stdin_bounded_on_a_non_live_state_is_a_defensive_control_error_and_leaves_state_unchanged(
    ) {
        // Misuse guard: calling write_stdin_bounded on NoPipe/TimedOut must
        // NEVER fabricate a timeout — it must restore the ORIGINAL state
        // unchanged and return a defensive Control error. This is pure logic
        // (no real ChildStdin needed since the Live branch is never reached).
        let mut state = StdinState::NoPipe;
        let err = write_stdin_bounded(&mut state, b"x", Duration::from_millis(50)).unwrap_err();
        assert!(matches!(err, BackendError::Control { op: "stdin", .. }));
        assert!(
            matches!(state, StdinState::NoPipe),
            "state must be unchanged"
        );

        let mut state = StdinState::TimedOut;
        let err = write_stdin_bounded(&mut state, b"x", Duration::from_millis(50)).unwrap_err();
        assert!(matches!(err, BackendError::Control { op: "stdin", .. }));
        assert!(
            matches!(state, StdinState::TimedOut),
            "an already-TimedOut state must stay TimedOut, not be reinterpreted"
        );
    }

    // ---- Story 4-2: rotation-decision logic + the synchronous append ----

    #[test]
    fn should_rotate_at_the_exact_boundary() {
        // The >= convention this codebase uses everywhere else (BudgetEvaluator).
        assert!(
            !should_rotate(LOG_ROTATE_MAX_BYTES - 1),
            "one under: no rotate"
        );
        assert!(should_rotate(LOG_ROTATE_MAX_BYTES), "exactly at: rotate");
        assert!(should_rotate(LOG_ROTATE_MAX_BYTES + 1), "one over: rotate");
    }

    #[test]
    fn generation_path_appends_the_generation_suffix() {
        let base = PathBuf::from("/tmp/agents/svc/logs/output.log");
        assert_eq!(
            generation_path(&base, 1),
            PathBuf::from("/tmp/agents/svc/logs/output.log.1")
        );
        assert_eq!(
            generation_path(&base, 2),
            PathBuf::from("/tmp/agents/svc/logs/output.log.2")
        );
    }

    fn line(text: &str) -> LogLine {
        LogLine::new("svc", LogStream::AgentOut, text, now_rfc3339())
    }

    #[test]
    fn append_attributed_lines_on_an_empty_batch_is_a_harmless_no_op() {
        // Defensive guard: tail_new_lines never calls this with an empty
        // batch in practice (it returns early itself when `consumable == 0`),
        // but append_attributed_lines is a directly-callable free function,
        // so its own empty-input guard is exercised here directly rather
        // than left as dead code — must not create the file or rotate.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("output.log");
        append_attributed_lines(&base, &[]);
        assert!(!base.exists(), "an empty batch must not create the file");
    }

    #[test]
    fn append_attributed_line_rotates_at_the_byte_bound_keeping_exactly_3_generations() {
        // Task 2: a LogLine sequence through the writer-thread primitive
        // rotates at the REAL LOG_ROTATE_MAX_BYTES bound and produces exactly
        // 3 generations with the oldest discarded — driven directly (no real
        // process, no wall-clock dependency), per the Testing Notes'
        // guidance. should_rotate is checked on size BEFORE each append, so
        // writing three ~4MB lines crosses the 10MB bound on the THIRD
        // append's pre-check (~8MB < 10MB, so it lands, leaving the
        // generation ~12MB) — the FOURTH append then rotates. Six lines total
        // exercises exactly one rotation.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("output.log");
        let big_text = "x".repeat(4 * 1024 * 1024); // 4MB text per line
        for i in 0..6 {
            append_attributed_line(&base, &line(&format!("{big_text}-{i}")));
        }
        assert!(base.exists(), "current generation must exist");
        assert!(
            generation_path(&base, 1).exists(),
            "at least one rotation must have produced a .1 generation"
        );
        // Never more than LOG_ROTATE_GENERATIONS - 1 rotated predecessors.
        assert!(
            !generation_path(&base, LOG_ROTATE_GENERATIONS).exists(),
            "no generation beyond LOG_ROTATE_GENERATIONS - 1 may survive"
        );
    }

    #[test]
    fn rotate_generations_discards_the_oldest_and_shifts_the_rest() {
        // A focused, deterministic proof of the rotation SEQUENCE itself
        // (independent of should_rotate's byte-bound timing): current -> .1,
        // .1 -> .2, prior .2 discarded.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("output.log");
        std::fs::write(&base, "current").unwrap();
        std::fs::write(generation_path(&base, 1), "gen1").unwrap();
        std::fs::write(generation_path(&base, 2), "gen2-oldest").unwrap();

        rotate_generations(&base, LOG_ROTATE_GENERATIONS);

        assert!(!base.exists(), "current was renamed away");
        assert_eq!(
            std::fs::read_to_string(generation_path(&base, 1)).unwrap(),
            "current"
        );
        assert_eq!(
            std::fs::read_to_string(generation_path(&base, 2)).unwrap(),
            "gen1"
        );
        // The prior .2 ("gen2-oldest") is gone — discarded, not shifted to a
        // nonexistent .3.
        assert!(!generation_path(&base, 3).exists());
    }

    #[test]
    fn rotate_generations_on_missing_predecessors_is_a_harmless_no_op() {
        // The FIRST-ever rotation: no `.1`/`.2` exist yet. Must not error or
        // panic on the missing rename/remove sources.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("output.log");
        std::fs::write(&base, "current").unwrap();
        rotate_generations(&base, LOG_ROTATE_GENERATIONS);
        assert!(!base.exists());
        assert_eq!(
            std::fs::read_to_string(generation_path(&base, 1)).unwrap(),
            "current"
        );
    }

    #[test]
    fn rotate_generations_with_a_degenerate_generation_count_just_drops_current() {
        // Defensive-only in production (LOG_ROTATE_GENERATIONS is a fixed 3),
        // but `rotate_generations` is a plain, directly-callable pure-ish
        // function, so its `generations <= 1` branch is exercised directly
        // here rather than left as dead code.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("output.log");
        std::fs::write(&base, "current").unwrap();
        rotate_generations(&base, 1);
        assert!(!base.exists(), "the only generation is simply dropped");
        assert!(!generation_path(&base, 1).exists());

        let base2 = dir.path().join("output2.log");
        std::fs::write(&base2, "current2").unwrap();
        rotate_generations(&base2, 0);
        assert!(!base2.exists());
    }

    #[test]
    fn synchronous_append_preserves_order_for_a_burst_of_same_second_lines() {
        // AC-G: multiple lines appended within one wall-clock second (the
        // SAME `at` value, since now_rfc3339 has whole-second resolution)
        // must land on disk in CALL order — never re-sorted. Fix pass
        // (review of #80): `append_attributed_line` is now the DIRECT,
        // synchronous write primitive (no channel/writer-thread hop to test
        // separately) — a single thread's sequential calls trivially
        // preserve order (each call fully completes, including its own
        // rotate-check, before the next begins), which is exactly what
        // makes this property hold with no additional synchronization
        // needed for the single-caller case.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("output.log");
        let same_at = "2026-07-15T00:00:00Z";
        for i in 0..20 {
            append_attributed_line(
                &base,
                &LogLine::new("svc", LogStream::AgentOut, format!("line-{i}"), same_at),
            );
        }

        let contents = std::fs::read_to_string(&base).unwrap();
        let texts: Vec<String> = contents
            .lines()
            .map(|l| {
                let parsed: LogLine = serde_json::from_str(l).unwrap();
                assert_eq!(parsed.at, same_at, "every line shares the same second");
                parsed.text
            })
            .collect();
        let want: Vec<String> = (0..20).map(|i| format!("line-{i}")).collect();
        assert_eq!(texts, want, "append order must equal call order");
    }

    #[test]
    fn log_capture_send_engine_line_is_safe_under_concurrent_callers() {
        // Fix pass (H1, review of #80): the whole point of guarding both
        // the tail cursors AND the rotate-then-append sequence with ONE
        // Mutex is that MULTIPLE concurrent callers (here: several cloned
        // `LogCapture` handles, standing in for the tailer thread racing an
        // inline `send_engine_line` call) can never interleave a
        // rotate/append and corrupt or lose a line — every line sent must
        // land, intact, parseable, exactly once.
        let dir = tempfile::tempdir().unwrap();
        let stdout_raw = dir.path().join("agent.log");
        let stderr_raw = dir.path().join("agent-stderr.log");
        let attributed = dir.path().join("output.log");
        std::fs::write(&stdout_raw, b"").unwrap();
        std::fs::write(&stderr_raw, b"").unwrap();
        let capture = spawn_output_capture(
            stdout_raw,
            stderr_raw,
            attributed.clone(),
            "svc".to_string(),
        );
        capture.signal_stop(); // no background tailer ticks racing this test

        const THREADS: usize = 8;
        const PER_THREAD: usize = 25;
        thread::scope(|scope| {
            for t in 0..THREADS {
                let capture = capture.clone();
                scope.spawn(move || {
                    for i in 0..PER_THREAD {
                        capture.send_engine_line(LogLine::new(
                            "svc",
                            LogStream::Engine,
                            format!("t{t}-{i}"),
                            now_rfc3339(),
                        ));
                    }
                });
            }
        });
        drop(capture);

        let contents = std::fs::read_to_string(&attributed).unwrap();
        let mut seen: Vec<String> = Vec::new();
        for l in contents.lines() {
            let parsed: LogLine = serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("every line must parse intact, never torn: {e}: {l:?}"));
            seen.push(parsed.text);
        }
        assert_eq!(
            seen.len(),
            THREADS * PER_THREAD,
            "every concurrently-sent line must land exactly once, none lost or duplicated"
        );
        let mut want: Vec<String> = (0..THREADS)
            .flat_map(|t| (0..PER_THREAD).map(move |i| format!("t{t}-{i}")))
            .collect();
        seen.sort();
        want.sort();
        assert_eq!(
            seen, want,
            "the exact SET of lines must match (order across threads is unspecified)"
        );
    }

    // ---- Fix pass (review of #80): the crash-immune raw-file tailer ----

    /// Read back an attributed capture file into parsed [`LogLine`]s
    /// (test-only helper; `read_agent_log`'s production equivalent lives in
    /// `domain::supervisor`, which this cfg-free module does not depend on).
    fn read_attributed(path: &Path) -> Vec<LogLine> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn tail_new_lines_reads_only_complete_lines_leaving_a_partial_tail() {
        // A trailing line with no '\n' yet must NOT be consumed this pass
        // (mirrors plan_follow's "only complete lines" rule) — proven
        // directly against the raw file, no thread needed.
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("stdout.raw");
        let attributed = dir.path().join("output.log");
        std::fs::write(&raw, b"a\nb\nc").unwrap(); // "c" has no trailing '\n'
        let mut cursor = 0u64;
        tail_new_lines(&raw, &mut cursor, LogStream::AgentOut, "svc", &attributed);
        let lines = read_attributed(&attributed);
        assert_eq!(
            lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"],
            "the partial trailing line ('c', no newline yet) must wait for the next pass"
        );
        assert_eq!(
            cursor, 4,
            "cursor advances only past the two complete lines"
        );

        // Appending the missing newline (+ more) makes "c" complete NOW.
        let mut file = std::fs::OpenOptions::new().append(true).open(&raw).unwrap();
        use std::io::Write as _;
        writeln!(file, "\nd").unwrap();
        drop(file);
        tail_new_lines(&raw, &mut cursor, LogStream::AgentOut, "svc", &attributed);
        let lines2 = read_attributed(&attributed);
        assert_eq!(
            lines2
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c", "d"],
            "the now-complete 'c' plus the new 'd' line must both be observed, appended after 'a'/'b'"
        );
    }

    #[test]
    fn tail_new_lines_is_idempotent_when_nothing_new_since_the_last_call() {
        // A redundant catch-up call (no growth since the last one) is a
        // harmless no-op — safe for the tailer thread and an inline
        // send_engine_line catch-up to both call this without duplicating
        // content.
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("stdout.raw");
        let attributed = dir.path().join("output.log");
        std::fs::write(&raw, b"a\nb\n").unwrap();
        let mut cursor = 0u64;
        tail_new_lines(&raw, &mut cursor, LogStream::AgentOut, "svc", &attributed);
        assert_eq!(cursor, 4);
        // Call again with NO new bytes written — must append nothing further.
        tail_new_lines(&raw, &mut cursor, LogStream::AgentOut, "svc", &attributed);
        let lines = read_attributed(&attributed);
        assert_eq!(
            lines.len(),
            2,
            "the redundant second call must not re-append already-tailed lines: {lines:?}"
        );
    }

    #[test]
    fn tail_new_lines_shrink_guard_snaps_the_cursor_without_reattributing() {
        // Defensive guard (should not normally happen for an append-only raw
        // capture): if the file is somehow shorter than the cursor, snap
        // forward and read nothing, rather than re-reading from the start
        // (which would re-attribute already-seen bytes as new).
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("stdout.raw");
        let attributed = dir.path().join("output.log");
        std::fs::write(&raw, b"a\nb\nc\n").unwrap();
        let mut cursor = 100u64; // artificially past the (real) 6-byte length
        tail_new_lines(&raw, &mut cursor, LogStream::AgentOut, "svc", &attributed);
        assert_eq!(cursor, 6, "cursor snaps to the file's actual length");
        assert!(
            !attributed.exists() || read_attributed(&attributed).is_empty(),
            "nothing must be (mis)appended on a shrink"
        );
    }

    #[test]
    fn tail_new_lines_stays_fast_against_a_huge_backlog_of_tiny_lines() {
        // Regression guard (fix pass, review of #80): a fast, bursty writer
        // with ZERO backpressure (a real OS process writing directly to its
        // own file redirect — exactly the crash-immune mechanism this fix
        // pass introduces) can accumulate a backlog of MANY tiny lines
        // between polls. This was EMPIRICALLY caught by this fix pass's own
        // crash-kill experiment (a `yes` process, writing "y\n" as fast as
        // the OS allows): a single `tail_new_lines` pass over a ~4MB/2M-line
        // backlog took ~15.7s in an unoptimized debug build BEFORE
        // MAX_TAIL_LINES_PER_PASS existed (bytes alone did not bound the
        // per-line LogLine/JSON-serialization work) — unacceptable for a
        // call that may run SYNCHRONOUSLY while the engine-wide supervisor
        // lock is held ([`LogCapture::send_engine_line`]). One pass over
        // the SAME shape of backlog must now complete in, at most, a small
        // fraction of a second — asserted generously (2s) to stay robust on
        // a slow/loaded CI runner while still catching a regression back to
        // the old unbounded-per-line behavior (which was 1000x+ slower).
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("stdout.raw");
        let attributed = dir.path().join("output.log");
        let big = "y\n".repeat((MAX_TAIL_BYTES_PER_PASS as usize) / 2 + 10);
        std::fs::write(&raw, big.as_bytes()).unwrap();
        let mut cursor = 0u64;
        let t0 = std::time::Instant::now();
        tail_new_lines(&raw, &mut cursor, LogStream::AgentOut, "svc", &attributed);
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "a single tail_new_lines pass must stay fast even against a huge tiny-line backlog, \
             took {elapsed:?} (pre-fix: ~15.7s in debug)"
        );
        // The line-count bound (not the byte bound) is what limits THIS
        // pass, since 2-byte lines make the byte bound reach far more
        // lines than MAX_TAIL_LINES_PER_PASS allows in one pass.
        assert_eq!(
            cursor,
            (MAX_TAIL_LINES_PER_PASS * 2) as u64,
            "capped by MAX_TAIL_LINES_PER_PASS lines, not the byte bound, for this tiny-line shape"
        );
    }

    #[test]
    fn log_capture_send_engine_line_catches_up_pending_raw_content_first() {
        // The ordering guarantee this fix pass adds: send_engine_line must
        // fold in whatever raw content already exists BEFORE appending its
        // own Engine line, so the engine line lands AFTER prior agent
        // output rather than racing the background tailer thread's own
        // poll schedule.
        let dir = tempfile::tempdir().unwrap();
        let stdout_raw = dir.path().join("agent.log");
        let stderr_raw = dir.path().join("agent-stderr.log");
        let attributed = dir.path().join("output.log");
        std::fs::write(&stdout_raw, b"").unwrap();
        std::fs::write(&stderr_raw, b"").unwrap();

        // Start the capture pipeline FIRST (its tailer cursor anchors at
        // the CURRENT, still-empty file length — mirroring how a real spawn
        // anchors before the child can write anything), THEN write raw
        // stdout content — simulating the agent emitting output that has
        // NOT yet been tailed by the time a transition happens.
        let capture = spawn_output_capture(
            stdout_raw.clone(),
            stderr_raw.clone(),
            attributed.clone(),
            "svc".to_string(),
        );
        // Signal stop IMMEDIATELY (before the tailer's own 20ms poll can
        // fire) so THIS test's own send_engine_line call is what performs
        // the catch-up, not a lucky tailer tick — a deterministic proof of
        // send_engine_line's own inline catch-up, not the tailer's.
        capture.signal_stop();
        std::fs::write(&stdout_raw, b"heartbeat 0\nheartbeat 1\n").unwrap();
        capture.send_engine_line(LogLine::new(
            "svc",
            LogStream::Engine,
            "engine: running -> stopped",
            now_rfc3339(),
        ));
        drop(capture);

        // Fix pass (H1, review of #80): send_engine_line writes
        // SYNCHRONOUSLY (no background writer thread/channel involved) —
        // by the time it returns, both the caught-up raw content and the
        // engine line are already durably on disk, so a single direct read
        // suffices; no polling needed.
        let contents = std::fs::read_to_string(&attributed).unwrap();
        let lines: Vec<LogLine> = contents
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let streams: Vec<LogStream> = lines.iter().map(|l| l.stream).collect();
        assert_eq!(
            streams,
            vec![LogStream::AgentOut, LogStream::AgentOut, LogStream::Engine],
            "the pre-existing raw content must be caught up BEFORE the engine line: {lines:?}"
        );
    }

    #[test]
    fn log_capture_signal_stop_lets_the_tailer_thread_exit_promptly() {
        // The tailer thread must not leak forever once its instance is
        // done: signaling stop makes it perform one final pass and return,
        // joinable deterministically (no wall-clock guess). Constructs a
        // `LogCapture` directly (rather than via `spawn_output_capture`,
        // which starts its OWN internal tailer) so this test can spawn and
        // `join` an INDEPENDENT tailer thread for one.
        let dir = tempfile::tempdir().unwrap();
        let stdout_raw = dir.path().join("agent.log");
        let stderr_raw = dir.path().join("agent-stderr.log");
        std::fs::write(&stdout_raw, b"").unwrap();
        std::fs::write(&stderr_raw, b"").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let capture = LogCapture {
            instance: "svc".to_string(),
            stdout_raw,
            stderr_raw,
            attributed_log_path: dir.path().join("output.log"),
            cursors: Arc::new(Mutex::new(TailCursors::default())),
            stop: Arc::clone(&stop),
        };
        let handle = spawn_tailer_thread(capture);
        stop.store(true, Ordering::Relaxed);
        // join() blocks until the thread actually returns — a hang here
        // would fail the test via the harness's own timeout, proving the
        // thread does NOT loop forever once stop is set.
        handle.join().expect("tailer thread must not panic");
    }
}
