//! The Unix [`ProcessBackend`] (spine AD-4) — process groups + signals.
//!
//! Spawns each Agent Instance into its OWN process group (a fresh session via
//! `setsid`, so the child is the group leader and `pgid == pid`). Stopping then
//! signals the WHOLE group with `killpg`, catching any child processes the agent
//! itself spawned — the load-bearing mechanism behind AC3 "no process of the
//! instance survives". Graceful stop is `SIGTERM` to the group; after the window
//! elapses it escalates to `SIGKILL` to the group, then CONFIRMS death bounded
//! to [`crate::ports::KILL_CONFIRM_TIMEOUT`] (fix pass, review of #80
//! follow-up — the CRITICAL finding: see that constant's docs for why
//! confirmation can no longer assume SIGKILL is always near-instant, and
//! `ProcessBackend::stop`'s docs for the full mechanism).
//!
//! This module is the allowlisted home for OS-conditional code (it is
//! `#[cfg(unix)]`-gated at its `mod` declaration in `backends/mod.rs`). It uses
//! `nix` for `setsid`/`kill`/`killpg` and `std::os::unix` for the `pre_exec`
//! child hook.
//!
//! ## Start-time fingerprint + orphan adoption (story 1-6, spine AD-5)
//!
//! [`UnixBackend::fingerprint`] reads a process's start-time — a per-boot,
//! per-process token that differs across a PID reuse — so the write-ahead spawn
//! record can carry `{ pid, start-time }` (AD-5). The SOURCE is per-OS and lives
//! ONLY here (the OS-cfg allowlist): Linux reads `/proc/<pid>/stat` field 22
//! (`starttime`, clock ticks since boot); macOS/BSD reads the process
//! `p_starttime` (a `timeval`, folded to microseconds) via `sysctl`
//! `KERN_PROC_PID`. Both are documented, stable APIs — no undocumented calls.
//! [`UnixBackend::adopt`] re-acquires a live PID whose current start-time equals
//! the recorded fingerprint (the PID-reuse guard), rebuilding a HANDLE that can
//! still signal/stop the group via `killpg` even though this process is not the
//! child's parent (so it holds no reap-able [`Child`]; liveness is `kill(pid, 0)`
//! and the OS/init reaps the non-child on exit). An adopted process is supervised
//! for the new engine's lifetime exactly like a freshly spawned one.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, killpg, Signal};
use nix::unistd::{setsid, Pid};

use crate::ports::{
    spawn_output_capture, write_stdin_bounded, BackendError, LogCapture, ProcessBackend,
    ProcessFingerprint, ProcessStatus, SecretError, SpawnSpec, StdinState, StopOutcome,
    KILL_CONFIRM_TIMEOUT, STDIN_WRITE_TIMEOUT,
};

/// How often the graceful-stop wait polls for the process to exit.
///
/// A short poll interval keeps a fast-exiting process from waiting the whole
/// window while bounding the busy-work. The wait runs on tokio's blocking pool
/// (the engine calls `stop` via `spawn_blocking`), so a bounded `sleep` here
/// does not stall an async worker.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A running process on Unix: the (optionally owned) child + its process-group
/// id.
///
/// The group id equals the child pid (the child is its own group leader via
/// `setsid`). For a FRESHLY SPAWNED process, `child` is `Some` — holding the
/// [`Child`] lets us reap it (no zombie). For an ADOPTED process (story 1-6:
/// re-acquired on engine start), `child` is `None` — this engine is not the
/// process's parent, so it cannot `wait()`/reap it; liveness is probed with
/// `kill(pid, 0)` and the OS/init reaps the non-child when it exits. In BOTH
/// cases the group id lets us signal the whole tree (`killpg`), so stop / pause /
/// resume work identically.
///
/// ## Steady-state PID-reuse guard for an ADOPTED handle (AI-10)
///
/// An adopted handle carries the LIVE `start_time` verified at adoption so its
/// liveness poll can RE-check the start-time fingerprint, not just the bare PID.
/// A spawned handle records its start-time too (for symmetry / diagnostics), but
/// it reaps through its owned [`Child`] so it is already immune to PID reuse; the
/// re-check matters only on the adopted (`child: None`) path.
#[derive(Debug)]
pub struct UnixProcess {
    /// The owned child handle, if THIS engine spawned the process (reaped on
    /// stop / drop). `None` for an adopted process (not our child).
    child: Option<Child>,
    /// The process-group id to signal (== child pid).
    pgid: Pid,
    /// The child pid, cached for diagnostics and the 1-6 adoption fingerprint.
    pid: u32,
    /// The recorded process start-time token (spine AD-5) — the PID-reuse guard
    /// carried on the handle itself. For a SPAWNED handle this was VERIFIED at
    /// spawn (AI-14: a failed read FAILS the spawn, so the `0` sentinel is never
    /// written by the engine's own paths). For an ADOPTED process (`child: None`)
    /// this is the LIVE start-time verified at adoption (always non-zero on the
    /// supported Linux/macOS hosts, since `adopt` returns `None` when it cannot
    /// read one), so [`UnixProcess::reap_if_exited`] can RE-verify it on every
    /// steady-state poll: a bare `kill(pid, 0)` alone cannot tell an adopted
    /// agent's crash from the OS recycling its PID to an unrelated process within
    /// a reaper interval (AI-10). `0` survives ONLY as the degraded fallback for
    /// a host with no start-time source (kept intact — the AI-10 fallback
    /// semantics are unchanged), reachable today only via a directly constructed
    /// handle.
    start_time: u64,
    /// The child's stdin channel state (story 4.1, spine AD-12; fix pass —
    /// CRITICAL/HIGH findings, review of #79). `Live` only for a FRESHLY
    /// SPAWNED process whose declared `Capability::Interaction` was
    /// `Guaranteed`/`BestEffort` on this OS (`SpawnSpec::pipe_stdin`);
    /// `NoPipe` for an ADOPTED process (a pipe fd cannot be recovered from a
    /// bare PID on any OS without an undocumented hack this project's
    /// conventions already reject — parity with the Windows-pause
    /// `[ASSUMPTION]`) or a freshly spawned one that was never piped
    /// (interaction `Unsupported`); `TimedOut` once a bounded write on this
    /// handle has exceeded [`STDIN_WRITE_TIMEOUT`] and can never be safely
    /// retried. `send_input` on anything but a `Live` state must therefore
    /// fail honestly (`EngineError::InteractionUnavailable` /
    /// `EngineError::InteractionTimedOut`), never silently succeed.
    stdin: StdinState,
    /// The output-capture pipeline handle (story 4-2, AD-12; fix pass,
    /// review of #80), if this handle has one. `Some` for a FRESHLY SPAWNED
    /// handle whenever the caller gave us somewhere to capture
    /// (`SpawnSpec::log_file`/`stderr_log_file`/`attributed_log_path`);
    /// `None` for an ADOPTED process — no live tailer thread survives the
    /// engine process that spawned it (parity with `stdin`'s
    /// `NoPipe`-on-adoption). This is NOT a functional gap for `kt agent
    /// logs`/`--follow` (AC-H): reading only needs the crash-immune raw
    /// FILES, which the agent process itself keeps writing to directly
    /// (never through any engine-held handle) for as long as it lives.
    log_capture: Option<LogCapture>,
}

/// The Unix process backend (AD-4).
///
/// Stateless — each running process is owned by its [`UnixProcess`] handle, held
/// by the supervisor. Constructed via [`crate::backends::current`].
#[derive(Clone, Copy, Debug, Default)]
pub struct UnixBackend;

impl UnixBackend {
    /// Construct the backend.
    pub fn new() -> Self {
        UnixBackend
    }
}

impl ProcessBackend for UnixBackend {
    type Handle = UnixProcess;

    fn spawn(&self, spec: &SpawnSpec) -> Result<Self::Handle, BackendError> {
        let mut command = Command::new(&spec.exec);
        command.args(&spec.args);
        command.current_dir(&spec.working_dir);
        // Apply env overrides on top of the inherited environment.
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        // Story 4-2 (AD-12, AC-E), fix pass (review of #80): stdout/stderr
        // capture is UNCONDITIONAL and capability-independent — each stream
        // is redirected DIRECTLY to its OWN regular file (crash-immune, NOT
        // a pipe: see the module docs on `spawn_output_capture` for why)
        // whenever the caller gave us somewhere to write all THREE capture
        // destinations (every PRODUCTION spawn does; the supervisor always
        // computes `log_file`/`stderr_log_file`/`attributed_log_path`
        // together from the SAME Registry path authority). This does NOT
        // gate on `Capability::Interaction` or any other capability —
        // reading FROM a process is never capability-gated, only writing TO
        // it (`pipe_stdin` below) is. `None` (all three fields) stays
        // `Stdio::null()`, unchanged from before this story — a narrow
        // test-fixture convenience for the small number of unit tests in
        // this module that assert nothing about captured output, not a
        // product-facing escape hatch (real spawns never take this
        // branch).
        debug_assert!(
            spec.log_file.is_some() == spec.attributed_log_path.is_some()
                && spec.log_file.is_some() == spec.stderr_log_file.is_some(),
            "SpawnSpec's three capture-path fields must be all Some or all None together"
        );
        let capture = match (
            &spec.log_file,
            &spec.stderr_log_file,
            &spec.attributed_log_path,
        ) {
            (Some(stdout_raw), Some(stderr_raw), Some(attributed)) => {
                Some((stdout_raw.clone(), stderr_raw.clone(), attributed.clone()))
            }
            _ => None,
        };
        // Fail FAST (mirrors the pre-story eager log_file-open validation)
        // if any destination cannot be opened — never a silent no-capture
        // outcome an operator would only notice from an unexpectedly-empty
        // log later. `stdout_target`/`stderr_target` are the SAME open
        // `File`s handed directly to `Stdio::from` below (a successful open
        // IS the fail-fast proof — no second reopen needed, unlike the old
        // dual-stream-into-one-file mechanism this superseded, which needed
        // a `try_clone` because two streams shared one file); the
        // attributed path is validated then dropped (whichever of the
        // background tailer thread or an inline `send_engine_line` call
        // reopens it per-append — see `append_attributed_lines`'s docs).
        let (stdout_target, stderr_target) = match &capture {
            Some((stdout_raw, stderr_raw, attributed)) => {
                let open = |path: &std::path::Path, label: &str| {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .map_err(|e| BackendError::Spawn {
                            exec: spec.exec.clone(),
                            detail: format!("could not open {label} {}: {e}", path.display()),
                        })
                };
                let stdout_file = open(stdout_raw, "log file")?;
                let stderr_file = open(stderr_raw, "stderr log file")?;
                drop(open(attributed, "attributed output log")?);
                (Some(stdout_file), Some(stderr_file))
            }
            None => (None, None),
        };
        // DIRECT, crash-immune redirects (never `Stdio::piped()` — the whole
        // point of this fix pass): the agent's `write()` to either stream
        // succeeds or fails based ONLY on this regular file, never on
        // whether the engine process is even still alive to read anything.
        command.stdout(match stdout_target {
            Some(file) => Stdio::from(file),
            None => Stdio::null(),
        });
        command.stderr(match stderr_target {
            Some(file) => Stdio::from(file),
            None => Stdio::null(),
        });
        // Piped ONLY when the caller (the supervisor, at spawn time) resolved
        // the declared Capability::Interaction level to Guaranteed/BestEffort
        // on this OS (story 4.1 fix pass, HIGH finding — review of #79;
        // supersedes the story's original unconditional `Stdio::piped()`).
        // An adapter that declares no interaction support gets Stdio::null()
        // — the pre-story-4.1 safe default — so a process that blocks
        // reading stdin at startup (a common "sniff for piped input" real-CLI
        // idiom) sees immediate EOF instead of hanging forever on a pipe
        // whose write end this engine holds open for its whole supervised
        // lifetime. This file stays free of capability LOOKUPS (the
        // supervisor already resolved the bool into `spec.pipe_stdin`); it
        // only branches on the resolved value.
        command.stdin(if spec.pipe_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        });

        // Put the child in its OWN session+process group BEFORE exec, so a later
        // killpg reaches the whole tree. `setsid` fails only if the caller is
        // already a group leader — never true for a freshly forked child — so
        // this is safe. `pre_exec` runs in the forked child after fork, before
        // exec: it must be async-signal-safe, which a bare setsid syscall is.
        //
        // SAFETY: the closure performs only an async-signal-safe syscall
        // (`setsid`) and allocates nothing; it does not touch shared state of
        // the parent. This satisfies the `pre_exec` contract.
        unsafe {
            command.pre_exec(|| {
                setsid().map_err(io::Error::from)?;
                Ok(())
            });
        }

        let mut child = command.spawn().map_err(|e| BackendError::Spawn {
            exec: spec.exec.clone(),
            detail: e.to_string(),
        })?;
        let pid = child.id();
        // The child is its own group leader, so pgid == pid.
        let pgid = Pid::from_raw(pid as i32);
        // Record the start-time for the write-ahead fingerprint (AD-5). AI-14:
        // the read is VERIFIED — a failure FAILS the spawn (fail closed) instead
        // of recording the `start_time = 0` sentinel, which would silently
        // downgrade every later orphan adoption to a pid-only match (the
        // PID-reuse guard would never fire).
        //
        // Loop-2 hardening: the raw std `Child` does NOT kill on drop, so a
        // failed read must kill the fresh group EXPLICITLY before returning —
        // this child is setsid-detached, and an early return without the kill
        // would orphan it with no handle and no record. A read that fails
        // because the pid is ALREADY GONE is not a platform failure either:
        // the agent exited instantly at startup, and THAT fact is surfaced.
        // The few-attempt retry absorbs the spawn-race window (a read racing
        // the child's own registration) without papering over a real outage.
        let start_time = {
            let mut token = None;
            for attempt in 0..START_TIME_READ_ATTEMPTS {
                match process_start_time(pid) {
                    Some(t) => {
                        token = Some(t);
                        break;
                    }
                    None if attempt + 1 < START_TIME_READ_ATTEMPTS => {
                        // A child that already exited will never yield a token
                        // — bail out of the retries immediately.
                        if matches!(child.try_wait(), Ok(Some(_))) {
                            break;
                        }
                        std::thread::sleep(START_TIME_READ_RETRY_DELAY);
                    }
                    None => {}
                }
            }
            match token {
                Some(t) => t,
                None => {
                    // Fail closed — but never orphan the child: SIGKILL the
                    // group this engine just created, then reap our child.
                    let _ = killpg(pgid, Signal::SIGKILL);
                    let _ = child.wait();
                    let platform_err = match verified_spawn_start_time(pid, &spec.exec) {
                        Err(err) => err,
                        // Unreachable: the loop above just failed this read on
                        // every attempt.
                        Ok(_) => unreachable!("the start-time read failed in the retry loop"),
                    };
                    return Err(match child.try_wait() {
                        // The pid was ALREADY GONE: the agent exited instantly
                        // at startup — surface that, not a platform failure.
                        Ok(Some(_)) => BackendError::Spawn {
                            exec: spec.exec.clone(),
                            detail: format!(
                                "the spawned agent exited immediately, before its process \
                                 start-time could be read for pid {pid} — the agent failed \
                                 at startup (check its logs); this is not a platform \
                                 start-time-source failure"
                            ),
                        },
                        // Child still alive after every attempt: the platform
                        // genuinely has no usable start-time source.
                        _ => platform_err,
                    });
                }
            }
        };
        // Capture the piped stdin now, for a FRESHLY SPAWNED handle only
        // (story 4.1) — `child.stdin` is `Some` exactly when `spec.pipe_stdin`
        // was true above (Stdio::piped() populates it; Stdio::null() never
        // does), so branching on std's own answer is simpler and more robust
        // than re-deriving it from `spec.pipe_stdin` a second time. An
        // adopted handle never has one (see `adopt` below).
        let stdin = match child.stdin.take() {
            Some(s) => StdinState::Live(s),
            None => StdinState::NoPipe,
        };
        // Story 4-2 (Task 3), fix pass (review of #80): a FRESHLY SPAWNED
        // handle gets the output-capture pipeline whenever `capture` is
        // `Some`. `spawn_output_capture` takes only the raw files' PATHS
        // (never `child.stdout`/`child.stderr` — those stay `None` here,
        // since neither stream was piped) — the tailer it starts reopens
        // them by path on every poll, exactly like any later reader would,
        // which is what makes this crash-immune: nothing depends on a
        // handle this engine session holds.
        let log_capture = capture.map(|(stdout_raw_path, stderr_raw_path, attributed_log_path)| {
            spawn_output_capture(
                stdout_raw_path,
                stderr_raw_path,
                attributed_log_path,
                spec.instance_name.clone(),
            )
        });
        Ok(UnixProcess {
            child: Some(child),
            pgid,
            pid,
            start_time,
            stdin,
            log_capture,
        })
    }

    fn stop(
        &self,
        handle: &mut Self::Handle,
        graceful_window: Duration,
    ) -> Result<StopOutcome, BackendError> {
        // Already gone? Reap and report a graceful (non-forced) stop.
        if handle.reap_if_exited()?.is_exited() {
            return Ok(StopOutcome { forced: false });
        }

        // (1) Graceful: SIGTERM to the whole group.
        signal_group(handle.pgid, Signal::SIGTERM)?;

        // (2) Wait up to the window for the group leader (our child) to exit.
        let deadline = Instant::now() + graceful_window;
        loop {
            if handle.reap_if_exited()?.is_exited() {
                // Exited gracefully within the window. Best-effort sweep of any
                // lingering group members the agent spawned, so none survive.
                //
                // [ASSUMPTION] pgid-reuse micro-window (documented, low severity;
                // parity with the Windows backend's assign-after-spawn honesty).
                // We reap the group LEADER first (the line just above), so by the
                // time this sweep runs the leader's pid — which equals the pgid —
                // has been released and the kernel could in principle recycle it
                // for an unrelated new group, which this SIGKILL would then hit.
                // The window is on the order of microseconds and bounded by
                // reaping the leader before sweeping; it is fully closed by the
                // pid + start-time fingerprint that story 1-6's orphan adoption
                // adds (a recycled group would fail the fingerprint match). No
                // behavior change here — this only records the known boundary.
                let _ = killpg(handle.pgid, Signal::SIGKILL);
                return Ok(StopOutcome { forced: false });
            }
            if Instant::now() >= deadline {
                break;
            }
            sleep(STOP_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
        }

        // (3) Escalate: SIGKILL to the whole group, then CONFIRM death —
        // bounded to KILL_CONFIRM_TIMEOUT (fix pass, review of #80
        // follow-up — the CRITICAL finding; see its docs for the full
        // mechanism: removing the pipe also removed its incidental
        // backpressure, so a fast writer can exhaust disk and enter an
        // OS-level UNINTERRUPTIBLE I/O wait immune to every signal,
        // including SIGKILL, until the underlying I/O resolves at the
        // kernel/storage layer). SIGKILL cannot be caught/ignored by a
        // NORMAL process, so the ONLY reason confirmation would take longer
        // than a few milliseconds is exactly that stuck-I/O scenario.
        //
        // A single unified polling loop via `reap_if_exited` (NON-BLOCKING
        // on both the spawned — `try_wait` — and adopted — `kill(pid, 0)` +
        // fingerprint re-check — branches) replaces the OLD unbounded
        // `child.wait()` (spawned) / already-informally-bounded-but-
        // dishonest `while pid_is_alive` (adopted, which used to silently
        // return `Ok` even when still alive after its own 5s poll). Group
        // members are killed by the SIGKILL below and reaped by init (or,
        // for a still-stuck leader, will be once the OS condition clears).
        signal_group(handle.pgid, Signal::SIGKILL)?;
        confirm_death(handle, KILL_CONFIRM_TIMEOUT)?;
        Ok(StopOutcome { forced: true })
    }

    fn poll(&self, handle: &mut Self::Handle) -> Result<ProcessStatus, BackendError> {
        handle.reap_if_exited()
    }

    fn pause(&self, handle: &mut Self::Handle) -> Result<(), BackendError> {
        // GUARANTEED pause on Unix (AC1): SIGSTOP the WHOLE process group. SIGSTOP
        // cannot be caught or ignored, so the suspension is real and verifiable
        // (the group freezes — a heartbeat stops growing). Guard for liveness
        // first (parity with `stop`): a process that already exited cannot be
        // paused, and that is a harmless no-op — the desired end state already
        // holds (and SIGSTOP to a gone group would resolve to ESRCH→Ok anyway).
        if handle.reap_if_exited()?.is_exited() {
            return Ok(());
        }
        signal_group(handle.pgid, Signal::SIGSTOP)
    }

    fn resume(&self, handle: &mut Self::Handle) -> Result<(), BackendError> {
        // GUARANTEED resume on Unix (AC1): SIGCONT the whole group, waking every
        // process the SIGSTOP suspended. Same already-exited tolerance as
        // `pause` — a resume of a gone process is a harmless no-op.
        if handle.reap_if_exited()?.is_exited() {
            return Ok(());
        }
        signal_group(handle.pgid, Signal::SIGCONT)
    }

    fn pid(&self, handle: &Self::Handle) -> u32 {
        handle.pid
    }

    fn fingerprint(&self, handle: &Self::Handle) -> ProcessFingerprint {
        // The handle's OWN start-time token — VERIFIED at spawn (AI-14: the spawn
        // FAILS when the read fails, so a spawned handle always carries a real,
        // non-zero token) or at adoption (`adopt` returns `None` when it cannot
        // read one). Using the held token means no second, separately-fallible
        // read here that could silently produce a `0` sentinel on the
        // write-ahead record, and it is byte-identical to what `spawn` verified.
        ProcessFingerprint::new(handle.pid, handle.start_time)
    }

    fn adopt(
        &self,
        fingerprint: &ProcessFingerprint,
    ) -> Result<Option<Self::Handle>, BackendError> {
        // The PID-reuse guard (AD-5): a live pid whose CURRENT start-time equals
        // the recorded one is the SAME process → adopt it. A gone pid, or one
        // whose start-time differs (reused for a new process), is NOT a match →
        // Ok(None), so the caller reconciles the record to `failed`.
        if !pid_is_alive(fingerprint.pid) {
            return Ok(None);
        }
        let live_start = match process_start_time(fingerprint.pid) {
            Some(t) => t,
            // The pid is alive but we cannot read its start-time (it may have
            // exited between the liveness probe and the read, or it is not
            // introspectable). Treat as no confident match — reconcile to failed.
            None => return Ok(None),
        };
        if live_start != fingerprint.start_time {
            // PID reused by a different process — do NOT adopt.
            return Ok(None);
        }
        // Same process. Rebuild an adopted handle: no owned Child (not our
        // child), but the group id (== pid, the setsid leader) lets us still
        // signal/stop the whole tree. Liveness is kill(pid, 0) RE-checked against
        // `live_start` on every poll (AI-10), so a later PID reuse cannot mask a
        // crash of this adopted process.
        //
        // `stdin: StdinState::NoPipe` (story 4.1): an adopted handle has no
        // recoverable pipe — there is no OS-portable, documented way to
        // reopen a `ChildStdin` from a bare pid. `send_input` against this
        // handle fails with `EngineError::InteractionUnavailable`, never
        // silently succeeding.
        //
        // `log_capture: None` (story 4-2, fix pass review of #80): no live
        // tailer thread survives the engine process that spawned it, so an
        // adopted handle gets no capture pipeline either. Reading/following
        // this instance's output still works (AC-H) — it needs only the
        // crash-immune raw FILES the agent process itself keeps writing to
        // directly, for as long as it lives, independent of any engine
        // session.
        Ok(Some(UnixProcess {
            child: None,
            pgid: Pid::from_raw(fingerprint.pid as i32),
            pid: fingerprint.pid,
            start_time: live_start,
            stdin: StdinState::NoPipe,
            log_capture: None,
        }))
    }

    fn has_stdin(&self, handle: &Self::Handle) -> bool {
        handle.stdin.is_live()
    }

    fn stdin_timed_out(&self, handle: &Self::Handle) -> bool {
        handle.stdin.is_timed_out()
    }

    fn write_stdin(&self, handle: &mut Self::Handle, data: &[u8]) -> Result<(), BackendError> {
        // Story 4.1 fix pass (CRITICAL finding, review of #79): bounded via
        // the shared, portable thread+channel+recv_timeout mechanism — see
        // `write_stdin_bounded`'s docs. Identical to the Windows backend's
        // body (this file and `backends/windows/mod.rs` intentionally share
        // ONE implementation via this call, rather than two separate OS
        // timeout implementations, since the mechanism has no OS-specific
        // part: a `ChildStdin` write is portable `std` on both).
        write_stdin_bounded(&mut handle.stdin, data, STDIN_WRITE_TIMEOUT)
    }

    fn log_capture(&self, handle: &Self::Handle) -> Option<LogCapture> {
        handle.log_capture.clone()
    }
}

/// Read a process's start-time — the per-boot, per-process token that differs
/// across a PID reuse (spine AD-5). The SOURCE is per-OS; both are documented,
/// stable APIs (no undocumented calls). Returns `None` if the process is gone or
/// its start-time cannot be read.
#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> Option<u64> {
    // Linux: /proc/<pid>/stat field 22 (`starttime`) — the time the process
    // started after boot, in clock ticks. The field is AFTER the `comm` field,
    // which is parenthesized and may itself contain spaces/parentheses, so we
    // split on the LAST ')' first, then take field 22 counting from there.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')').map(|(_, rest)| rest)?;
    // After ')' the fields are: state(3) ppid(4) ... starttime(22). `rest`
    // begins with a leading space then field 3, so index 22-3 = 19 in the
    // whitespace-split remainder.
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // field 3 is fields[0]; field 22 is fields[19].
    fields.get(19).and_then(|s| s.parse::<u64>().ok())
}

/// macOS: the process start-time (`pbi_start_tvsec`/`pbi_start_tvusec`) via the
/// documented `libproc` `proc_pidinfo(PROC_PIDTBSDINFO)` call, folded to
/// microseconds since the epoch — stable per process, different across a PID
/// reuse. Uses `nix::libc` (nix re-exports libc; no new dependency). No
/// undocumented API. Behavior-verified on the macOS CI leg.
#[cfg(target_os = "macos")]
fn process_start_time(pid: u32) -> Option<u64> {
    use nix::libc;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a zeroed proc_bsdinfo of exactly `size` bytes;
    // proc_pidinfo writes at most `size` bytes into it. The pointer is valid for
    // the call's duration. proc_pidinfo returns the number of bytes written.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if written != size {
        // Process gone, insufficient permission, or a short read — no confident
        // start-time.
        return None;
    }
    // Fold seconds + microseconds into a single microsecond token — stable per
    // process, distinct across a PID reuse.
    let secs = info.pbi_start_tvsec as i128;
    let usec = info.pbi_start_tvusec as i128;
    let micros = secs.saturating_mul(1_000_000).saturating_add(usec);
    if micros <= 0 {
        return None;
    }
    Some(micros as u64)
}

/// Other Unix (neither Linux nor macOS — e.g. a BSD without `libproc`): no
/// supported start-time source. Returns `None`. With AI-14's fail-closed spawn
/// this means a host with no start-time source cannot spawn AT ALL (every spawn
/// fails with a clear diagnostic) — the honest consequence: without a start-time
/// token the write-ahead record's PID-reuse guard cannot work, and silently
/// recording a pid-only fingerprint (the old `0` sentinel) would make every
/// later adoption decision untrustworthy. Ktesio's supported targets are
/// Linux / macOS / Windows; this arm only keeps the backend COMPILING on other
/// Unix.
#[cfg(all(unix, not(target_os = "linux"), not(target_os = "macos")))]
fn process_start_time(_pid: u32) -> Option<u64> {
    None
}

/// AI-14: resolve a just-spawned child's start-time token — the write-ahead
/// spawn record's PID-reuse guard (AD-5) — FAILING the spawn when the read
/// fails instead of recording the `start_time = 0` sentinel. Fail closed: a
/// record without a real token would silently downgrade every later orphan
/// adoption to a pid-only match (the reused-PID guard would never fire), so the
/// engine refuses the spawn up front with a clear diagnostic naming the pid.
/// (Loop 2: the CALLER owns the retries, the exited-instantly distinction, and
/// killing the fresh group before surfacing this error — see the `spawn` body.)
/// The unit is the same opaque per-OS token [`process_start_time`] reads.
fn verified_spawn_start_time(pid: u32, exec: &str) -> Result<u64, BackendError> {
    process_start_time(pid).ok_or_else(|| BackendError::Spawn {
        exec: exec.to_string(),
        detail: format!(
            "could not read the process start-time for the spawned pid {pid} — no usable \
             process start-time source on this platform; cannot guarantee pid-reuse safety, \
             so refusing to record a start-time-less fingerprint (sentinel 0): the \
             write-ahead spawn record needs the real token for orphan adoption"
        ),
    })
}

/// AI-14 (loop 2): how many times the spawn path attempts the start-time read
/// before failing closed, and how long it waits between attempts — enough to
/// absorb the spawn-race window (a read racing the child's registration in the
/// process table) without masking a genuine platform outage for any real
/// duration.
const START_TIME_READ_ATTEMPTS: usize = 3;
const START_TIME_READ_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);

impl Drop for UnixProcess {
    /// Kill the process group on drop so a dropped handle never leaks the agent
    /// (or any child it spawned). This is what keeps `kt agent start` — which
    /// spawns, records `running`, then EXITS — from orphaning the process: when
    /// the supervisor is torn down at engine shutdown, the handle drops and the
    /// whole group dies. Best-effort; a group already gone is fine.
    ///
    /// NOTE (cross-lifetime, AD-5 is story 1-6): a handle for a SPAWNED process
    /// is cleaned up when this engine ends (kill-on-drop). An ADOPTED handle
    /// (re-acquired on engine start) likewise SIGKILLs its group on drop, but
    /// cannot `wait` a non-child (the OS/init reaps it); this keeps the
    /// no-survivor guarantee across engine restarts.
    ///
    /// Fix pass (review of #80): ALSO signals the output-capture pipeline's
    /// background tailer thread to stop (one final catch-up pass, then
    /// exit) — unconditionally, regardless of whether the process itself
    /// was still alive at this point, so a tailer thread never outlives its
    /// instance (no per-stop/restart thread leak over a long engine
    /// session). This is purely a LOCAL bookkeeping signal — it never
    /// touches the agent process and has NOTHING to do with its crash
    /// resilience, which comes entirely from the raw capture files being
    /// direct, engine-independent OS redirects (see `spawn_output_capture`'s
    /// docs).
    fn drop(&mut self) {
        if let Some(capture) = &self.log_capture {
            capture.signal_stop();
        }
        // If already reaped/exited, nothing to do; otherwise SIGKILL the group.
        if let Ok(ProcessStatus::Alive) = self.reap_if_exited() {
            let _ = killpg(self.pgid, Signal::SIGKILL);
            // Reap the direct child if it is ours (adopted: init reaps it).
            if let Some(child) = self.child.as_mut() {
                let _ = child.wait();
            }
        }
    }
}

impl UnixProcess {
    /// Non-blocking: reap the child if it has exited, returning its status.
    ///
    /// For a SPAWNED process (`child: Some`) this is `try_wait` (which reaps on
    /// exit, no zombie). For an ADOPTED process (`child: None`, story 1-6) this
    /// engine is not the parent and cannot `wait`, so liveness is `kill(pid, 0)`:
    /// `ESRCH` means the process is gone (reaped by its real parent / init), any
    /// other result means it is still alive. An adopted process reports
    /// `Exited { code: None }` when gone — we cannot recover a non-child's exit
    /// code, which is honest (the code is unknown to us).
    ///
    /// ## Adopted PID-reuse guard (AI-10)
    ///
    /// A bare `kill(pid, 0)` cannot distinguish "the adopted process is still
    /// alive" from "the adopted process crashed and the OS recycled its PID for an
    /// unrelated new process within a reaper interval (~250ms)" — both read as
    /// alive, so the crash would be missed and the row left a phantom `running`.
    /// So when this handle carries a real recorded start-time (`start_time != 0`,
    /// always true for an adopted handle on the supported Linux/macOS hosts — see
    /// [`UnixBackend::adopt`]), we RE-read the live PID's start-time and treat a
    /// MISMATCH or a read failure as "the original process is gone" →
    /// `Exited { code: None }`. Only when we have NO recorded token (`start_time
    /// == 0`, a degraded host with no start-time source) do we fall back to the
    /// bare liveness probe — the best that host can honestly do. A spawned handle
    /// (`child: Some`) reaps via its owned `Child` and never reaches this path, so
    /// it was already immune to PID reuse.
    fn reap_if_exited(&mut self) -> Result<ProcessStatus, BackendError> {
        match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => Ok(ProcessStatus::Exited {
                    code: status.code(),
                }),
                Ok(None) => Ok(ProcessStatus::Alive),
                Err(e) => Err(BackendError::Control {
                    op: "wait",
                    detail: e.to_string(),
                }),
            },
            // Adopted (not our child): probe liveness via kill(pid, 0), then
            // RE-verify the start-time fingerprint so a recycled PID cannot mask a
            // crash (AI-10).
            None => {
                if !pid_is_alive(self.pid) {
                    // The PID is gone → the process exited (reaped by init).
                    return Ok(ProcessStatus::Exited { code: None });
                }
                // The PID is alive. If we hold a real recorded start-time, confirm
                // it still matches the live PID.
                if self.start_time != 0 {
                    match process_start_time(self.pid) {
                        // Successful read that MATCHES → the same process, alive.
                        Some(live) if live == self.start_time => Ok(ProcessStatus::Alive),
                        // Successful read that MISMATCHES → the PID was recycled for
                        // a DIFFERENT process; the original adopted process is gone.
                        // This is THE AI-10 guard: a recycled PID always yields a
                        // successful read + a differing start-time, so this path is
                        // exactly the recycled-PID case.
                        Some(_) => Ok(ProcessStatus::Exited { code: None }),
                        // Read FAILED (a transient /proc or libproc hiccup) → we
                        // CANNOT confidently distinguish alive-vs-gone, so do NOT
                        // report a crash on an ambiguous read (that would spuriously
                        // kill+restart a genuinely-live agent). Treat as still-alive
                        // and let the reaper re-check next tick — a truly-dead
                        // process is still caught then (its PID goes away → Exited,
                        // or a reused PID reads a MISMATCHING start-time → Exited),
                        // so this does NOT reopen the AI-10 hole. This mirrors how
                        // `adopt`/`fingerprint` already treat a start-time read
                        // failure as "not confident", never as a positive signal.
                        None => Ok(ProcessStatus::Alive),
                    }
                } else {
                    // Degraded host with no start-time source: bare liveness is the
                    // honest best we can do (an adopted handle here is unusual —
                    // `adopt` needs a readable start-time to create one at all).
                    Ok(ProcessStatus::Alive)
                }
            }
        }
    }
}

/// Whether a pid is still alive: `kill(pid, 0)` succeeds (or fails with `EPERM`
/// — alive but not ours to signal) while it lives, and fails with `ESRCH` once
/// it is gone. Used for adopted-process liveness (no reap-able [`Child`]).
fn pid_is_alive(pid: u32) -> bool {
    match kill(Pid::from_raw(pid as i32), None) {
        Err(nix::errno::Errno::ESRCH) => false,
        // Ok (we can signal it) or EPERM (alive but not ours) → alive.
        _ => true,
    }
}

/// Signal a whole process group, treating "no such process" as success.
///
/// `killpg` returns `ESRCH` if the group is already gone — the desired end state
/// for a stop, so it is NOT an error. Any other failure is a real control error.
fn signal_group(pgid: Pid, signal: Signal) -> Result<(), BackendError> {
    match killpg(pgid, signal) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::ESRCH) => Ok(()), // group already gone
        Err(e) => Err(BackendError::Control {
            op: "signal",
            detail: e.to_string(),
        }),
    }
}

/// Poll `handle` (via the NON-BLOCKING [`UnixProcess::reap_if_exited`]) until
/// it reports exited, or `timeout` elapses — the bounded death-confirmation
/// primitive [`UnixBackend::stop`]'s escalation phase uses (fix pass, review
/// of #80 follow-up — the CRITICAL finding).
///
/// `timeout` is a PARAMETER (never hardcoded in this function) so it stays
/// directly unit-testable with a SHORT duration for fast, deterministic
/// coverage of the bound-enforcement logic itself — mirrors
/// [`write_stdin_bounded`]'s existing "timeout as a parameter, tested
/// directly with a short value" precedent (story 4.1 fix pass); production
/// calls this with [`KILL_CONFIRM_TIMEOUT`]. Returns `Ok(())` once confirmed
/// dead; [`BackendError::StopUnconfirmed`] if `timeout` elapses first
/// (naming the ACTUAL `timeout` passed, so a short test timeout reports
/// itself honestly rather than always claiming the production bound); any
/// other [`BackendError`] from `reap_if_exited` (e.g. an unexpected `wait`
/// failure) propagates unchanged.
fn confirm_death(handle: &mut UnixProcess, timeout: Duration) -> Result<(), BackendError> {
    let deadline = Instant::now() + timeout;
    loop {
        if handle.reap_if_exited()?.is_exited() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            // Release the caller (never keep blocking): the process is NOT
            // confirmed dead — it may be alive, stuck. The caller
            // (`Supervisor::stop_inner`) must not claim `stopped`.
            return Err(BackendError::StopUnconfirmed {
                timeout_secs: timeout.as_secs(),
            });
        }
        sleep(STOP_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// Check that the engine secrets file at `path` is owner-only (mode `0600`),
/// story 2-4 AC6 (spine AD-10/AD-4 — the OS-specific permission INSPECTION, which
/// MUST live under `backends/`, the sole allowlisted `#[cfg]` home).
///
/// On Unix this READS the file's mode bits via
/// [`std::os::unix::fs::PermissionsExt`] and REFUSES a group/other-accessible file
/// (a world-/group-readable secrets file defeats "safe by construction"): a typed
/// [`SecretError::FilePermissions`] with a `chmod 600` remediation. The RULE
/// (`mode & 0o077 == 0`) + the error construction live in the OS-agnostic
/// [`crate::ports`] ([`crate::ports::mode_is_owner_only`] /
/// [`crate::ports::file_permissions_error`]) so they are unit-testable without a
/// real file; this function only supplies the mode-bit READ (the OS-specific part).
/// The caller (the file resolver) has already confirmed the file exists; a stat
/// failure surfaces as [`SecretError::FileUnreadable`].
pub fn check_secrets_file_permissions(path: &std::path::Path) -> Result<(), SecretError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).map_err(|e| SecretError::FileUnreadable {
        path: path.to_string_lossy().into_owned(),
        detail: e.to_string(),
    })?;
    let mode = metadata.permissions().mode();
    if crate::ports::mode_is_owner_only(mode) {
        Ok(())
    } else {
        Err(crate::ports::file_permissions_error(path, mode))
    }
}

/// Copy an EXISTING target file's Unix permissions onto the freshly written
/// `temp` BEFORE the atomic rename (story 11-2 review-1, patch 3) — so an
/// operator-tightened mode (e.g. a hand-chmod'ed `0600 config.toml`) survives
/// every overwrite instead of silently widening back to the process default.
/// A target that does not exist (the first write) is a no-op — the temp keeps
/// the process-default mode. Stat/permission ERRORS are surfaced (a failed
/// mode copy must not silently publish a widened or narrowed file); they run
/// before the rename, so a failure leaves the previous target untouched.
///
/// Lives here (the sole allowlisted `#[cfg]` home) because the mode-bit APIs
/// are `std::os::unix`-only; the caller ([`crate::paths::write_atomic_via`])
/// stays cfg-free. Re-exported cfg-selected from `backends/mod.rs`, exactly
/// like [`check_secrets_file_permissions`].
pub fn preserve_target_mode(target: &std::path::Path, temp: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(target) {
        Ok(meta) => std::fs::set_permissions(
            temp,
            std::fs::Permissions::from_mode(meta.permissions().mode()),
        ),
        // First write: no previous mode to preserve.
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Rename the temp over the atomic-write target (story 11-2 review-1, patch 4).
/// POSIX `rename` replaces ANY existing non-directory target atomically and is
/// never blocked by another process holding the target open (an open fd does
/// not share-block a rename), so there is nothing transient to retry — errors
/// surface immediately.
pub fn rename_over_target(temp: &std::path::Path, target: &std::path::Path) -> io::Result<()> {
    std::fs::rename(temp, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{LogLine, LogStream};
    use std::collections::BTreeMap;

    /// Resolve the `fake_agent` helper binary via the conformance dev-dependency
    /// (a dev-dep of the engine — off the shipping graph). Public within the test
    /// module so the child-survivor test can reach it.
    pub(super) fn fake_agent_path() -> std::path::PathBuf {
        ktesio_conformance::fake_agent_bin()
    }

    fn spec(exec: &str, args: &[&str]) -> SpawnSpec {
        SpawnSpec {
            exec: exec.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
            working_dir: std::env::temp_dir(),
            log_file: None,
            // No output-capture wanted (story 4-2): this helper is used by
            // tests that assert nothing about captured output, mirroring
            // `log_file: None`'s existing "don't care" convention exactly.
            attributed_log_path: None,
            stderr_log_file: None,
            instance_name: "test".to_string(),
            // Preserves this shared helper's pre-fix behavior (piping was
            // unconditional before the HIGH fix): every test using this
            // helper spawns with a live stdin pipe, matching what they
            // already assumed.
            pipe_stdin: true,
        }
    }

    #[test]
    fn spawn_a_sleep_then_stop_kills_it() {
        // Spawn `sleep 60`, confirm it is alive, then stop with a short window.
        // It ignores nothing, so SIGTERM ends it gracefully (forced == false).
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&spec("sleep", &["60"])).expect("spawn sleep");
        assert!(proc.pid > 0);
        assert_eq!(backend.poll(&mut proc).unwrap(), ProcessStatus::Alive);

        let outcome = backend
            .stop(&mut proc, Duration::from_secs(5))
            .expect("stop");
        assert!(!outcome.forced, "sleep exits on SIGTERM without escalation");
        assert!(backend.poll(&mut proc).unwrap().is_exited());
    }

    #[test]
    fn spawn_missing_exec_is_a_spawn_error_no_zombie() {
        // AC2: a non-existent exec fails at spawn with a preserved diagnostic and
        // leaves no child to zombie (nothing was spawned).
        let backend = UnixBackend::new();
        let err = backend
            .spawn(&spec("ktesio-no-such-binary-xyz", &[]))
            .unwrap_err();
        match err {
            BackendError::Spawn { exec, detail } => {
                assert_eq!(exec, "ktesio-no-such-binary-xyz");
                assert!(!detail.is_empty());
            }
            other => panic!("expected Spawn, got {other}"),
        }
    }

    #[test]
    fn stop_an_already_exited_process_is_graceful() {
        // `true` exits immediately. By the time we stop, it is already gone; the
        // backend reaps it and reports a non-forced stop (desired end state).
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&spec("true", &[])).expect("spawn true");
        // Give it a moment to exit.
        sleep(Duration::from_millis(50));
        let outcome = backend
            .stop(&mut proc, Duration::from_secs(1))
            .expect("stop");
        assert!(!outcome.forced);
    }

    #[test]
    fn poll_reports_exit_of_a_short_lived_process() {
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&spec("true", &[])).expect("spawn true");
        // Poll until it exits (bounded).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if backend.poll(&mut proc).unwrap().is_exited() {
                break;
            }
            assert!(Instant::now() < deadline, "process did not exit in time");
            sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn log_file_captures_child_stdout() {
        // The spawned child's stdout is redirected to the per-instance log file.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("instance.log");
        let mut s = spec("echo", &["hello-from-child"]);
        s.log_file = Some(log.clone());
        s.attributed_log_path = Some(dir.path().join("output.log"));
        s.stderr_log_file = Some(dir.path().join("stderr.raw"));
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&s).expect("spawn echo");
        // Wait for it to exit.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !backend.poll(&mut proc).unwrap().is_exited() {
            assert!(Instant::now() < deadline);
            sleep(Duration::from_millis(10));
        }
        // Fix pass (review of #80): `log_file` is once again a DIRECT,
        // synchronous OS redirect (`Stdio::from(file)`, not a pipe an
        // engine-side reader thread drains on its own schedule) — so
        // "the child process has exited" once again implies "every byte it
        // wrote is already on disk," with NO reader-thread catch-up window
        // to poll for (the pre-story-4-2 guarantee, restored). A single
        // direct read suffices.
        let contents = std::fs::read_to_string(&log).unwrap();
        assert!(
            contents.contains("hello-from-child"),
            "content must be present immediately, no reader-thread lag: {contents:?}"
        );
    }

    #[test]
    fn pid_accessor_returns_the_child_pid() {
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&spec("sleep", &["30"])).expect("spawn");
        assert_eq!(backend.pid(&proc), proc.pid);
        assert!(backend.pid(&proc) > 0);
        // Teardown.
        let _ = backend.stop(&mut proc, Duration::from_secs(2));
    }

    #[test]
    fn fingerprint_is_stable_across_reads_for_the_same_process() {
        // AD-5: the fingerprint must be STABLE for a live process (two reads
        // agree) so a write-ahead record can be reconciled later. On the
        // supported hosts (Linux/macOS) the start-time is a real, non-zero token.
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&spec("sleep", &["30"])).expect("spawn");
        let fp1 = backend.fingerprint(&proc);
        let fp2 = backend.fingerprint(&proc);
        assert_eq!(fp1, fp2, "fingerprint must be stable across reads");
        assert_eq!(fp1.pid, proc.pid);
        // On the two supported Unix hosts the start-time source is real.
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(fp1.start_time > 0, "start-time token should be populated");
        }
        let _ = backend.stop(&mut proc, Duration::from_secs(2));
    }

    #[test]
    fn adopt_reacquires_a_live_matching_process_and_can_stop_it() {
        // AC7 (Unix): a live process whose start-time matches the recorded
        // fingerprint is ADOPTED — the re-held handle can still stop the group.
        // Spawn a long-lived child, capture its fingerprint, "adopt" it via a
        // second backend (simulating a new engine), and stop it through the
        // adopted handle. Leak-guard: keep the ORIGINAL handle so if adoption
        // fails the child is still reaped on drop.
        let backend = UnixBackend::new();
        let mut original = backend
            .spawn(&spec("sleep", &["600"]))
            .expect("spawn sleep");
        let fp = backend.fingerprint(&original);
        assert!(pid_alive(fp.pid));

        // Adopt via a fresh backend (the OS re-acquisition path).
        let adopter = UnixBackend::new();
        let adopted = adopter.adopt(&fp).expect("adopt call ok");
        let mut adopted = adopted.expect("a live matching process must be adopted");
        assert_eq!(adopter.pid(&adopted), fp.pid);
        assert_eq!(adopter.poll(&mut adopted).unwrap(), ProcessStatus::Alive);

        // Stopping through the ADOPTED handle terminates the process group.
        let _ = adopter.stop(&mut adopted, Duration::from_millis(500));
        // In the REAL orphan scenario the original engine has died, so the
        // process's parent is init, which auto-reaps it. Here the original
        // backend still owns the Child, so the killed process would linger as a
        // ZOMBIE until reaped; poll the original handle in the loop to reap it
        // (standing in for init) so `pid_alive` reflects the true end state.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // Reap via the original owner (init's role in production).
            let reaped = backend
                .poll(&mut original)
                .map(|s| s.is_exited())
                .unwrap_or(true);
            if reaped && !pid_alive(fp.pid) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "adopted process must be killable via the re-held handle"
            );
            sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn adopt_returns_none_for_a_gone_pid() {
        // AC7: a fingerprint whose pid is gone yields Ok(None) → the caller
        // reconciles the record to `failed`. Spawn `true`, let it exit, then try
        // to adopt its (now-dead) fingerprint.
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&spec("true", &[])).expect("spawn true");
        let fp = backend.fingerprint(&proc);
        // Wait for it to exit + reap.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !backend.poll(&mut proc).unwrap().is_exited() {
            assert!(Instant::now() < deadline);
            sleep(Duration::from_millis(10));
        }
        // Give the OS a moment to fully release the pid.
        sleep(Duration::from_millis(50));
        let adopted = backend.adopt(&fp).expect("adopt call ok");
        assert!(adopted.is_none(), "a gone pid must not be adopted");
    }

    #[test]
    fn adopt_returns_none_on_start_time_mismatch_pid_reuse_guard() {
        // AD-5 PID-reuse guard: a live pid whose start-time DIFFERS from the
        // recorded one is a DIFFERENT process (the pid was recycled) and must NOT
        // be adopted. Spawn a live process, then craft a fingerprint with its pid
        // but a deliberately wrong start-time.
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&spec("sleep", &["30"])).expect("spawn");
        let real = backend.fingerprint(&proc);
        // Only meaningful where a real start-time source exists.
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            let forged = ProcessFingerprint::new(real.pid, real.start_time.wrapping_add(1));
            let adopted = backend.adopt(&forged).expect("adopt call ok");
            assert!(
                adopted.is_none(),
                "a start-time mismatch (PID reuse) must not be adopted"
            );
        }
        let _ = backend.stop(&mut proc, Duration::from_secs(2));
    }

    #[test]
    fn adopted_poll_reports_exited_when_start_time_no_longer_matches_ai10() {
        // AI-10 (steady-state PID-reuse guard): an ADOPTED handle whose live PID is
        // still alive but whose start-time NO LONGER matches the recorded token is
        // a DIFFERENT process (the original crashed and the OS recycled its PID) —
        // `poll` must report `Exited`, NOT `Alive`, so the reaper detects the crash
        // instead of trusting a bare PID match.
        //
        // We prove this deterministically WITHOUT waiting for a real PID recycle:
        // adopt a genuinely live process (so the handle is a real adopted handle),
        // then construct a sibling handle for the SAME live pid whose recorded
        // start-time is deliberately wrong. A bare `kill(pid,0)` would still say
        // "alive"; the start-time re-check must override that to `Exited`.
        if !cfg!(any(target_os = "linux", target_os = "macos")) {
            return; // no real start-time source on this host — guard is a no-op.
        }
        let backend = UnixBackend::new();
        // Keep the ORIGINAL handle so the live process is reaped on drop (leak
        // guard) regardless of what the crafted handle reports.
        let mut original = backend
            .spawn(&spec("sleep", &["600"]))
            .expect("spawn sleep");
        let fp = backend.fingerprint(&original);
        assert!(fp.start_time > 0, "supported host has a real start-time");

        // A genuine adopted handle (correct start-time) polls Alive.
        let adopter = UnixBackend::new();
        let mut adopted = adopter
            .adopt(&fp)
            .expect("adopt call ok")
            .expect("a live matching process must be adopted");
        assert_eq!(
            adopter.poll(&mut adopted).unwrap(),
            ProcessStatus::Alive,
            "a matching adopted process is alive"
        );

        // Now craft an adopted-shaped handle for the SAME live pid but with a
        // recorded start-time that does NOT match the live one (models a recycled
        // PID). The pid is provably still alive (the original sleep is running).
        assert!(pid_alive(fp.pid), "the live pid is still running");
        let mut recycled = UnixProcess {
            child: None,
            pgid: Pid::from_raw(fp.pid as i32),
            pid: fp.pid,
            start_time: fp.start_time.wrapping_add(1),
            stdin: StdinState::NoPipe,
            log_capture: None,
        };
        assert_eq!(
            backend.poll(&mut recycled).unwrap(),
            ProcessStatus::Exited { code: None },
            "a live pid whose start-time no longer matches must poll Exited (AI-10), \
             not Alive — so a recycled PID cannot mask an adopted process's crash"
        );

        // Teardown the real process via the original owning handle.
        let _ = backend.stop(&mut original, Duration::from_secs(2));
    }

    #[test]
    fn adopted_poll_with_no_recorded_start_time_falls_back_to_bare_liveness() {
        // AI-10 degraded fallback: an adopted-shaped handle that carries NO
        // recorded start-time (`start_time == 0` — a host with no start-time
        // source, where `adopt` would not normally build a handle) must fall back
        // to the bare `kill(pid, 0)` liveness probe, not spuriously report Exited.
        // Construct such a handle over a genuinely live pid and assert Alive.
        let backend = UnixBackend::new();
        let mut original = backend
            .spawn(&spec("sleep", &["600"]))
            .expect("spawn sleep");
        let pid = original.pid;
        assert!(pid_alive(pid));

        let mut degraded = UnixProcess {
            child: None,
            pgid: Pid::from_raw(pid as i32),
            pid,
            start_time: 0, // no recorded token → bare-liveness fallback
            stdin: StdinState::NoPipe,
            log_capture: None,
        };
        assert_eq!(
            backend.poll(&mut degraded).unwrap(),
            ProcessStatus::Alive,
            "a live adopted pid with no recorded start-time falls back to bare liveness"
        );

        // Teardown via the real owner.
        let _ = backend.stop(&mut original, Duration::from_secs(2));
    }

    #[test]
    fn spawn_fails_closed_when_the_start_time_read_fails_ai14() {
        // AI-14: the spawn-site start-time read is VERIFIED — a failed read
        // FAILS the spawn with a clear Spawn diagnostic instead of recording the
        // `start_time = 0` sentinel. Proven through the SAME resolver `spawn`
        // uses: a pid with no readable start-time (here a pid that cannot be a
        // live process — outside every supported host's pid range) maps to a
        // Spawn error naming the pid, while a REAL child's read yields the
        // non-zero token the fingerprint then carries verbatim (so the
        // write-ahead record can never silently become pid-only).
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&spec("sleep", &["30"])).expect("spawn");
        let live = process_start_time(proc.pid).expect("a live child has a readable start-time");
        assert!(
            live > 0,
            "the verified token is the real one, never the 0 sentinel"
        );
        let err = verified_spawn_start_time(u32::MAX - 1, "sleep").unwrap_err();
        match &err {
            BackendError::Spawn { exec, detail } => {
                assert_eq!(exec, "sleep");
                assert!(detail.contains("start-time"), "names the failure: {detail}");
                assert!(detail.contains("4294967294"), "names the pid: {detail}");
                assert!(
                    detail.contains("sentinel 0"),
                    "says what it refused to record: {detail}"
                );
            }
            other => panic!("expected Spawn, got {other}"),
        }
        // The fingerprint comes from the handle's OWN verified token — identical
        // across reads, never a second fallible read that could produce a 0.
        assert_eq!(backend.fingerprint(&proc).start_time, live);
        assert_eq!(backend.fingerprint(&proc), backend.fingerprint(&proc));
        // Teardown.
        let _ = backend.stop(&mut proc, Duration::from_secs(2));
    }

    #[test]
    fn spawn_with_unopenable_log_file_is_a_spawn_error() {
        // The log_file cannot be opened (its parent is a regular file, not a
        // dir) → a Spawn error whose detail names the log-open failure, and
        // nothing is left running.
        let dir = tempfile::tempdir().unwrap();
        // Make `blocked` a FILE, then ask to log into `blocked/inner.log`.
        let blocker = dir.path().join("blocked");
        std::fs::write(&blocker, b"not a dir").unwrap();
        let mut s = spec("sleep", &["30"]);
        s.log_file = Some(blocker.join("inner.log"));
        // Valid attributed/stderr paths, so `capture` is considered wanted
        // and the eager validation actually runs (all three fields gate
        // capture together — see the backend's `spawn()` docs). `log_file`
        // is opened FIRST, so its failure is what surfaces.
        s.attributed_log_path = Some(dir.path().join("output.log"));
        s.stderr_log_file = Some(dir.path().join("stderr.raw"));
        let backend = UnixBackend::new();
        let err = backend.spawn(&s).unwrap_err();
        match err {
            BackendError::Spawn { detail, .. } => {
                assert!(detail.contains("log file"), "detail={detail}")
            }
            other => panic!("expected Spawn, got {other}"),
        }
    }

    #[test]
    fn signal_group_treats_missing_group_as_success() {
        // Signalling a group that does not exist returns Ok (ESRCH → success).
        // Use a pgid extremely unlikely to exist.
        let result = signal_group(Pid::from_raw(2_000_000_000), Signal::SIGTERM);
        assert!(result.is_ok(), "missing group must be Ok, got {result:?}");
    }

    #[test]
    fn check_secrets_file_permissions_refuses_group_readable_and_accepts_0600() {
        // Story 2-4 AC6 (Unix): a 0644 (group/other-readable) secrets file is
        // REFUSED with a typed FilePermissions error + chmod remediation; a 0600
        // (owner-only) file passes. This is the real enforcement of AD-10's "mode
        // 0600". Lives in the backend test module (the allowlisted OS-cfg home), so
        // reading Unix mode bits here needs no cfg elsewhere.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.toml");
        std::fs::write(&path, "OPENAI_KEY = \"x\"\n").unwrap();

        // 0644 → refused.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = check_secrets_file_permissions(&path).unwrap_err();
        match &err {
            crate::ports::SecretError::FilePermissions { path: p, detail } => {
                assert!(p.contains("secrets.toml"), "{p}");
                assert!(detail.contains("group/other"), "{detail}");
            }
            other => panic!("expected FilePermissions, got {other:?}"),
        }
        assert!(err.to_string().contains("chmod 600"), "{err}");

        // 0600 → accepted.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_secrets_file_permissions(&path).is_ok());

        // 0400 (read-only owner) is also owner-only → accepted.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(check_secrets_file_permissions(&path).is_ok());
    }

    /// Whether a pid is still alive (Unix): `kill(pid, 0)` succeeds while it
    /// lives, fails with ESRCH once it is gone. Test-only liveness probe.
    ///
    /// A ZOMBIE (defunct) process still answers `kill(pid, 0)` — it occupies its
    /// pid until reaped — so a bare `kill(0)` probe reports a just-SIGKILLed
    /// child as alive when there is no reaping PID1 (e.g. a bare CI container),
    /// false-failing the group-kill assertions. On Linux, after `kill(0)` says
    /// the pid exists, read `/proc/<pid>/stat` and treat process state `Z` as NOT
    /// alive. macOS keeps the plain `kill(0)` semantics (no /proc; its callers
    /// run under a reaping launchd). This is a non-destructive read — it never
    /// `waitpid`s, so the backend keeps sole ownership of reaping its children.
    fn pid_alive(pid: u32) -> bool {
        use nix::sys::signal::kill;
        let exists = !matches!(
            kill(Pid::from_raw(pid as i32), None),
            Err(nix::errno::Errno::ESRCH)
        );
        #[cfg(target_os = "linux")]
        if exists {
            // /proc/<pid>/stat is "pid (comm) state ...". `comm` may contain
            // spaces or ')', so the state code is the first token AFTER the
            // final ')'. A missing/unreadable stat (already reaped) is not a
            // zombie, so fall through to `exists`.
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                if let Some((_, rest)) = stat.rsplit_once(')') {
                    if rest.split_whitespace().next() == Some("Z") {
                        return false;
                    }
                }
            }
        }
        exists
    }

    #[test]
    fn stop_kills_the_whole_group_no_child_survivor() {
        // THE load-bearing AC3 test (Unix): spawn `fake_agent --spawn-child`,
        // which forks a lingering CHILD in the same process group, then stop the
        // group and assert BOTH the parent AND the child are gone. A naive
        // "kill the parent PID" would miss the child; the process-group SIGKILL
        // catches it. The child pid is read from the redirected agent log.
        let dir = tempfile::tempdir().unwrap();
        let agent_log = dir.path().join("agent.log");
        let bin = fake_agent_path();
        let mut s = SpawnSpec {
            exec: bin.to_string_lossy().into_owned(),
            args: vec![
                "--spawn-child".to_string(),
                "--linger-ms".to_string(),
                "600000".to_string(),
            ],
            env: BTreeMap::new(),
            working_dir: dir.path().to_path_buf(),
            log_file: Some(agent_log.clone()),
            attributed_log_path: Some(dir.path().join("output.log")),
            stderr_log_file: Some(dir.path().join("stderr.raw")),
            instance_name: "svc".to_string(),
            pipe_stdin: true,
        };
        s.env.clear();

        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&s).expect("spawn fake_agent --spawn-child");
        let parent_pid = proc.pid;

        // Wait for the child pid to be announced in the agent log.
        let child_pid = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(contents) = std::fs::read_to_string(&agent_log) {
                    if let Some(line) = contents.lines().find(|l| l.starts_with("child-pid=")) {
                        break line["child-pid=".len()..].trim().parse::<u32>().unwrap();
                    }
                }
                assert!(Instant::now() < deadline, "child pid never announced");
                sleep(Duration::from_millis(20));
            }
        };
        assert!(pid_alive(parent_pid), "parent should be alive before stop");
        assert!(pid_alive(child_pid), "child should be alive before stop");

        // Stop with a short window; the group SIGKILL must catch both.
        let outcome = backend
            .stop(&mut proc, Duration::from_millis(200))
            .expect("stop");
        // fake_agent ignores nothing, so SIGTERM should end the parent within the
        // window (graceful); regardless, no process survives.
        let _ = outcome;

        // Give the OS a moment to tear down the group.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !pid_alive(parent_pid) && !pid_alive(child_pid) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "parent(alive={}) or child(alive={}) survived the group kill",
                pid_alive(parent_pid),
                pid_alive(child_pid)
            );
            sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn stop_escalates_to_forced_kill_when_graceful_window_elapses() {
        // AC3 escalation (Unix): a process that outlives a SHORT graceful window
        // is force-killed (SIGKILL), and the outcome records forced == true. We
        // force this WITHOUT an OS-cfg signal handler by giving the process a
        // long linger and the stop a tiny window — so the graceful SIGTERM does
        // not matter (fake_agent has no handler, so SIGTERM actually ends it
        // fast). To truly exercise escalation we need the process to survive
        // SIGTERM; `sh -c 'trap "" TERM; sleep 60'` ignores SIGTERM portably in
        // POSIX sh, forcing the window to elapse and SIGKILL to fire.
        let backend = UnixBackend::new();
        let mut proc = backend
            .spawn(&spec("sh", &["-c", "trap '' TERM; sleep 60"]))
            .expect("spawn sh trap");
        // Let the trap install.
        sleep(Duration::from_millis(100));
        let outcome = backend
            .stop(&mut proc, Duration::from_millis(200))
            .expect("stop");
        assert!(
            outcome.forced,
            "a SIGTERM-ignoring process must be force-killed (escalation)"
        );
        assert!(backend.poll(&mut proc).unwrap().is_exited());
    }

    #[test]
    fn confirm_death_gives_up_honestly_within_the_bound_when_the_process_never_exits() {
        // Fix pass (review of #80 follow-up — the CRITICAL finding): a
        // deterministic, FAST proof of the bound-enforcement logic itself —
        // mirrors this codebase's EXISTING `write_stdin_bounded`-called-
        // directly-with-a-short-timeout pattern (story 4.1 fix pass), rather
        // than waiting out the real 5s `KILL_CONFIRM_TIMEOUT` production
        // bound. No genuine OS-level "immune to SIGKILL" state (the
        // uninterruptible-I/O-wait this fix pass targets, empirically
        // confirmed via a dedicated ramdisk experiment — see the story file's
        // Dev Agent Record) is needed to prove THIS property: `confirm_death`
        // only calls `reap_if_exited` and does not know or care WHY it keeps
        // reporting `Alive` — a process that is simply still ALIVE when a
        // SHORT test timeout elapses exercises the identical code path.
        // `confirm_death` is called DIRECTLY here (never sending SIGKILL) so
        // this test proves the WAIT-BOUNDING logic in isolation; the
        // escalation call site's actual SIGKILL + confirm_death sequencing
        // is covered by `stop_escalates_to_forced_kill_when_graceful_window_elapses`
        // above (a NORMAL process, confirmed quickly).
        let backend = UnixBackend::new();
        let mut proc = backend
            .spawn(&spec("sleep", &["30"]))
            .expect("spawn sleep 30");
        let start = Instant::now();
        let err = confirm_death(&mut proc, Duration::from_millis(150)).expect_err(
            "a still-alive process must not be reported confirmed dead within a short bound",
        );
        let elapsed = start.elapsed();
        match err {
            BackendError::StopUnconfirmed { timeout_secs } => {
                // 150ms rounds down to 0 whole seconds — proves the ACTUAL
                // timeout passed is what gets reported, not a hardcoded 5.
                assert_eq!(timeout_secs, 0, "reports the timeout actually passed");
            }
            other => panic!("expected StopUnconfirmed, got {other}"),
        }
        assert!(
            elapsed >= Duration::from_millis(150),
            "must honor the full bound before giving up: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "must not overshoot substantially: {elapsed:?}"
        );
        // Clean up: confirm_death only POLLS, it never signals, so the
        // process is still alive — actually stop it now.
        let _ = backend.stop(&mut proc, Duration::from_millis(200));
    }

    #[test]
    fn confirm_death_returns_promptly_once_the_process_actually_exits() {
        // The self-healing / no-compounding counterpart: once the process
        // DOES exit (here, naturally and quickly via `true`), confirm_death
        // must return Ok promptly rather than waiting out the full bound —
        // this is what lets a RETRY `stop()` (Supervisor::stop_inner) or the
        // crash reaper reconcile a previously-stuck instance quickly once
        // the underlying condition clears, instead of always paying the
        // full timeout.
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&spec("true", &[])).expect("spawn true");
        let start = Instant::now();
        confirm_death(&mut proc, Duration::from_secs(5)).expect("must confirm death");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "a quickly-exiting process must be confirmed well before the bound: {elapsed:?}"
        );
    }

    #[test]
    fn pause_freezes_the_process_then_resume_wakes_it() {
        // AC1 (Unix guaranteed): spawn `fake_agent --heartbeat-ms 50`, which
        // prints an incrementing `heartbeat <n>` line to its log every ~50ms.
        // While SIGSTOP'd the process freezes, so the log's line count STOPS
        // growing; SIGCONT resumes it and the count grows again. This is the
        // cross-Unix-safe suspension proof (no /proc dependency).
        let dir = tempfile::tempdir().unwrap();
        let agent_log = dir.path().join("agent.log");
        let bin = fake_agent_path();
        let mut s = SpawnSpec {
            exec: bin.to_string_lossy().into_owned(),
            args: vec![
                "--heartbeat-ms".to_string(),
                "50".to_string(),
                "--linger-ms".to_string(),
                "600000".to_string(),
            ],
            env: BTreeMap::new(),
            working_dir: dir.path().to_path_buf(),
            pipe_stdin: true,
            log_file: Some(agent_log.clone()),
            attributed_log_path: Some(dir.path().join("output.log")),
            stderr_log_file: Some(dir.path().join("stderr.raw")),
            instance_name: "svc".to_string(),
        };
        s.env.clear();

        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&s).expect("spawn fake_agent --heartbeat-ms");

        // Wait for the heartbeat to actually start ticking (a couple of lines).
        let line_count = |path: &std::path::Path| -> usize {
            std::fs::read_to_string(path)
                .map(|c| c.lines().filter(|l| l.starts_with("heartbeat ")).count())
                .unwrap_or(0)
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if line_count(&agent_log) >= 2 {
                break;
            }
            assert!(Instant::now() < deadline, "heartbeat never started");
            sleep(Duration::from_millis(20));
        }

        // Pause: SIGSTOP the group, then confirm the heartbeat is FROZEN.
        //
        // Robustness under scheduler jitter (LOW-3): rather than compare two
        // instantaneous samples (fragile when a loaded runner delays a read, or
        // when the child's in-flight 25ms poll lands one last line right after
        // pause returns), we settle briefly, snapshot a BASELINE, then watch
        // across a LONG window (1s ≫ many 50ms intervals) and require the count
        // NEVER exceeds baseline. A stuck-but-alive scheduler cannot make a
        // SUSPENDED process emit, so this tolerates jitter; yet it stays a
        // GENUINE proof — if the `pause()` (SIGSTOP) were removed, a live 50ms
        // heartbeat would emit ~20 lines here and exceed baseline on the first
        // poll, firing the assert. (Resume below further requires renewed growth.)
        backend.pause(&mut proc).expect("pause");
        sleep(Duration::from_millis(200)); // let SIGSTOP + any in-flight line settle
        let baseline = line_count(&agent_log);
        let watch_until = Instant::now() + Duration::from_millis(1000);
        while Instant::now() < watch_until {
            let now = line_count(&agent_log);
            assert!(
                now <= baseline,
                "heartbeat must NOT grow while paused (SIGSTOP) — baseline {baseline}, saw {now}"
            );
            sleep(Duration::from_millis(50));
        }
        let paused_after = line_count(&agent_log);
        assert_eq!(
            paused_after, baseline,
            "heartbeat count must be unchanged across the paused window: {baseline} → {paused_after}"
        );

        // Resume: SIGCONT the group; the heartbeat must grow again.
        backend.resume(&mut proc).expect("resume");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if line_count(&agent_log) > paused_after {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "heartbeat must resume growing after SIGCONT (stuck at {paused_after})"
            );
            sleep(Duration::from_millis(20));
        }

        // Teardown.
        let _ = backend.stop(&mut proc, Duration::from_millis(200));
    }

    #[test]
    fn pause_and_resume_on_an_already_exited_process_are_harmless_no_ops() {
        // A dead process cannot be paused/resumed; both are harmless no-ops
        // (parity with stop-on-dead). `true` exits immediately.
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&spec("true", &[])).expect("spawn true");
        sleep(Duration::from_millis(50));
        // Reap it via poll so the handle knows it exited.
        assert!(backend.poll(&mut proc).unwrap().is_exited());
        backend.pause(&mut proc).expect("pause on dead is Ok");
        backend.resume(&mut proc).expect("resume on dead is Ok");
    }

    #[test]
    fn working_dir_and_env_are_applied() {
        // Prove the working dir + env override reach the child: run `sh -c` that
        // writes $PWD and $KT_TEST into the log, and assert both.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("out.log");
        let mut s = SpawnSpec {
            exec: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "printf 'pwd=%s env=%s' \"$PWD\" \"$KT_TEST\"".to_string(),
            ],
            env: BTreeMap::new(),
            working_dir: dir.path().to_path_buf(),
            log_file: Some(log.clone()),
            attributed_log_path: Some(dir.path().join("output.log")),
            stderr_log_file: Some(dir.path().join("stderr.raw")),
            instance_name: "svc".to_string(),
            pipe_stdin: true,
        };
        s.env.insert("KT_TEST".to_string(), "applied".to_string());
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&s).expect("spawn sh");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !backend.poll(&mut proc).unwrap().is_exited() {
            assert!(Instant::now() < deadline);
            sleep(Duration::from_millis(10));
        }
        // Fix pass (review of #80): `log_file` is a DIRECT, synchronous OS
        // redirect again (see `log_file_captures_child_stdout`'s identical
        // comment) — "exited" already implies "fully written", no
        // reader-thread catch-up window to poll for.
        let contents = std::fs::read_to_string(&log).unwrap();
        assert!(contents.contains("env=applied"), "log={contents:?}");
        // The working dir is the temp dir (canonicalize to dodge /var→/private/var).
        let want = std::fs::canonicalize(dir.path()).unwrap();
        assert!(
            contents.contains(&format!("pwd={}", want.display()))
                || contents.contains(&format!("pwd={}", dir.path().display())),
            "log={contents:?} want pwd={}",
            want.display()
        );
    }

    #[test]
    fn write_stdin_delivers_a_line_that_is_echoed_into_the_captured_log() {
        // Story 4.1 Task 1: spawning `fake_agent --echo-stdin` and writing a
        // line via `write_stdin` produces the echoed `stdin: <line>` in the
        // captured log — the OS-level plumbing `Supervisor::send_input` relies
        // on.
        let dir = tempfile::tempdir().unwrap();
        let agent_log = dir.path().join("agent.log");
        let bin = fake_agent_path();
        let mut s = SpawnSpec {
            exec: bin.to_string_lossy().into_owned(),
            args: vec![
                "--echo-stdin".to_string(),
                "--linger-ms".to_string(),
                "600000".to_string(),
            ],
            env: BTreeMap::new(),
            working_dir: dir.path().to_path_buf(),
            log_file: Some(agent_log.clone()),
            attributed_log_path: Some(dir.path().join("output.log")),
            stderr_log_file: Some(dir.path().join("stderr.raw")),
            instance_name: "svc".to_string(),
            pipe_stdin: true,
        };
        s.env.clear();

        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&s).expect("spawn fake_agent --echo-stdin");
        assert!(
            backend.has_stdin(&proc),
            "a freshly spawned handle has a live stdin pipe"
        );
        assert!(
            !backend.stdin_timed_out(&proc),
            "a freshly spawned handle has not timed out"
        );

        backend
            .write_stdin(&mut proc, b"hello-stdin\n")
            .expect("write_stdin");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(contents) = std::fs::read_to_string(&agent_log) {
                if contents.lines().any(|l| l == "stdin: hello-stdin") {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "echoed stdin line never appeared"
            );
            sleep(Duration::from_millis(20));
        }

        let _ = backend.stop(&mut proc, Duration::from_millis(200));
    }

    #[test]
    fn has_stdin_is_true_when_spawned_and_false_once_adopted() {
        // Story 4.1 AC-D: a freshly spawned handle holds a live stdin pipe;
        // once re-acquired via `adopt` (simulating a DIFFERENT engine session
        // after a crash+restart), the pipe cannot be recovered — the adopted
        // handle must report `has_stdin() == false` so `send_input` fails
        // honestly (`InteractionUnavailable`) instead of silently succeeding.
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&spec("sleep", &["30"])).expect("spawn sleep");
        assert!(backend.has_stdin(&proc), "freshly spawned handle has stdin");
        assert!(!backend.stdin_timed_out(&proc), "not timed out yet");

        let fp = backend.fingerprint(&proc);
        let adopter = UnixBackend::new();
        let adopted = adopter
            .adopt(&fp)
            .expect("adopt call ok")
            .expect("a live matching process must be adopted");
        assert!(
            !adopter.has_stdin(&adopted),
            "an adopted handle has no recoverable stdin pipe"
        );
        assert!(
            !adopter.stdin_timed_out(&adopted),
            "an adopted handle never had a pipe, so it never timed out either — \
             distinct from a handle that HAD a live pipe and timed out"
        );

        let _ = backend.stop(&mut proc, Duration::from_secs(2));
    }

    #[test]
    fn write_stdin_without_a_live_pipe_is_a_control_error() {
        // Defensive path (callers MUST check `has_stdin` first): writing to a
        // handle with no live pipe (e.g. an adopted handle) is a
        // `BackendError::Control`, never a panic and never a silent success.
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&spec("sleep", &["30"])).expect("spawn");
        let fp = backend.fingerprint(&proc);
        let adopter = UnixBackend::new();
        let mut adopted = adopter
            .adopt(&fp)
            .expect("adopt call ok")
            .expect("a live matching process must be adopted");
        let err = adopter.write_stdin(&mut adopted, b"x\n").unwrap_err();
        match err {
            BackendError::Control { op, .. } => assert_eq!(op, "stdin"),
            other => panic!("expected Control, got {other}"),
        }
        let _ = backend.stop(&mut proc, Duration::from_secs(2));
    }

    // ---- Story 4.1 fix pass (CRITICAL finding, review of #79): the bounded
    // stdin write. These call `write_stdin_bounded` DIRECTLY with a short,
    // custom timeout (rather than through the `ProcessBackend::write_stdin`
    // trait method, which hardcodes the real production `STDIN_WRITE_TIMEOUT`
    // of 5s) so the MECHANISM is proven fast here; the full production
    // dispatch through `Supervisor::send_input` using the real 5s constant is
    // proven end-to-end (including the "a different instance is not blocked
    // beyond the bound" property, which needs the engine's actual shared
    // lock) by `crates/ktesio-engine/tests/interaction.rs`. ----

    #[test]
    fn write_stdin_bounded_times_out_on_a_non_draining_pipe_and_marks_the_handle_timed_out() {
        // A process that never reads its stdin (no --echo-stdin) and a
        // payload that comfortably exceeds any OS pipe buffer (Linux
        // defaults to 64KiB; even a generously tuned buffer is nowhere near
        // 8MB) — so the write blocks once the buffer fills, exactly the
        // adversarial audit's reproduction. A short 150ms custom timeout
        // (NOT the production 5s constant) keeps this test fast while still
        // exercising the real mechanism end to end.
        let backend = UnixBackend::new();
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_agent_path();
        let s = SpawnSpec {
            exec: bin.to_string_lossy().into_owned(),
            args: vec!["--linger-ms".to_string(), "600000".to_string()],
            env: BTreeMap::new(),
            working_dir: dir.path().to_path_buf(),
            log_file: Some(dir.path().join("agent.log")),
            attributed_log_path: Some(dir.path().join("output.log")),
            stderr_log_file: Some(dir.path().join("stderr.raw")),
            instance_name: "svc".to_string(),
            pipe_stdin: true,
        };
        let mut proc = backend
            .spawn(&s)
            .expect("spawn fake_agent (no --echo-stdin: never reads stdin)");
        assert!(backend.has_stdin(&proc), "freshly spawned handle has stdin");

        let huge_payload = vec![b'x'; 8 * 1024 * 1024]; // 8MB, far past any pipe buffer
        let start = Instant::now();
        let err = write_stdin_bounded(&mut proc.stdin, &huge_payload, Duration::from_millis(150))
            .expect_err("a write to a non-draining pipe past its buffer must time out");
        let elapsed = start.elapsed();
        match err {
            BackendError::StdinTimedOut { timeout_secs } => assert_eq!(timeout_secs, 0), // 150ms rounds to 0s
            other => panic!("expected StdinTimedOut, got {other}"),
        }
        assert!(
            elapsed < Duration::from_secs(2),
            "the bounded write must return promptly after its OWN short timeout, not hang: {elapsed:?}"
        );

        // The handle is now durably marked TimedOut: has_stdin is false, and
        // stdin_timed_out distinguishes this from "never had a pipe".
        assert!(!backend.has_stdin(&proc), "no longer reports a live pipe");
        assert!(
            backend.stdin_timed_out(&proc),
            "must be distinguishably TimedOut, not merely NoPipe"
        );

        // Teardown: killing the group unblocks the abandoned write thread
        // (its blocked write() call gets EPIPE once the reader is gone), so
        // it cleans itself up in the background; the test does not wait for
        // it (there is no safe way to join it — see write_stdin_bounded's
        // docs — and it is harmless to leave running briefly).
        let _ = backend.stop(&mut proc, Duration::from_millis(200));
    }

    #[test]
    fn write_stdin_after_a_timeout_is_a_defensive_control_error_with_no_new_write_attempted() {
        // Once a handle is TimedOut, a caller that (incorrectly) tries to
        // write again must get an IMMEDIATE defensive Control error — never
        // another attempted write, and never a second wait. This is the
        // backend-level safety net; `Supervisor::send_input` additionally
        // short-circuits via `stdin_timed_out` before ever reaching here
        // (proven at the engine level in interaction.rs), but this backend
        // method must be safe even if called directly.
        let backend = UnixBackend::new();
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_agent_path();
        let s = SpawnSpec {
            exec: bin.to_string_lossy().into_owned(),
            args: vec!["--linger-ms".to_string(), "600000".to_string()],
            env: BTreeMap::new(),
            working_dir: dir.path().to_path_buf(),
            log_file: Some(dir.path().join("agent.log")),
            attributed_log_path: Some(dir.path().join("output.log")),
            stderr_log_file: Some(dir.path().join("stderr.raw")),
            instance_name: "svc".to_string(),
            pipe_stdin: true,
        };
        let mut proc = backend.spawn(&s).expect("spawn fake_agent");

        // Force a fast timeout (150ms, not the production 5s) to reach the
        // TimedOut state quickly.
        let huge_payload = vec![b'x'; 8 * 1024 * 1024];
        let first = write_stdin_bounded(&mut proc.stdin, &huge_payload, Duration::from_millis(150));
        assert!(matches!(first, Err(BackendError::StdinTimedOut { .. })));
        assert!(backend.stdin_timed_out(&proc));

        // A second attempt (via the PUBLIC trait method this time, which
        // internally would use the real 5s bound if it attempted a write —
        // it must not) returns instantly.
        let start = Instant::now();
        let second = backend.write_stdin(&mut proc, b"more\n");
        let elapsed = start.elapsed();
        match second {
            Err(BackendError::Control { op, .. }) => assert_eq!(op, "stdin"),
            other => panic!("expected a defensive Control error, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_millis(500),
            "a write on an already-TimedOut handle must return immediately, not attempt \
             (and wait out) another doomed write: {elapsed:?}"
        );
        assert!(
            backend.stdin_timed_out(&proc),
            "must remain TimedOut, not be reset by the defensive no-op"
        );

        let _ = backend.stop(&mut proc, Duration::from_millis(200));
    }

    // ---- Story 4-2: capture-path wiring (AC-A, AC-B, AC-E) ----

    #[test]
    fn spawn_captures_both_streams_attributed_and_stdout_only_in_the_legacy_log() {
        // Task 3, fix pass (review of #80): spawning fake_agent with output
        // on BOTH streams produces attributed lines for BOTH in the NEW
        // attributed capture (agent-out/agent-err) — proving direct,
        // crash-immune per-stream redirects (`Stdio::from(file)`, never a
        // pipe) still feed full attribution via the background tailer.
        //
        // DELIBERATE CHANGE from this test's pre-fix-pass form (renamed
        // accordingly): the legacy `agent.log` is now DIRECTLY,
        // synchronously written by the OS for STDOUT ALONE (never merged
        // with stderr via any engine-side hop — see `SpawnSpec::log_file`'s
        // docs for why: `drain_usage_for`'s billing-critical sentinel is a
        // stdout-only convention, so keeping `agent.log` a PURE, zero-hop
        // stdout redirect is what fully closes H2, the fix pass's
        // billing-race finding). Stderr is captured to its OWN separate raw
        // file (`stderr_log_file`), asserted directly below, rather than
        // asserting it appears (it no longer does, by design) inside
        // `agent.log`.
        let dir = tempfile::tempdir().unwrap();
        let agent_log = dir.path().join("agent.log");
        let stderr_raw = dir.path().join("agent-stderr.log");
        let output_log = dir.path().join("output.log");
        let bin = fake_agent_path();
        let s = SpawnSpec {
            exec: bin.to_string_lossy().into_owned(),
            args: vec![
                "--heartbeat-ms".to_string(),
                "30".to_string(),
                "--heartbeat-stderr-ms".to_string(),
                "30".to_string(),
                "--linger-ms".to_string(),
                "600000".to_string(),
            ],
            env: BTreeMap::new(),
            working_dir: dir.path().to_path_buf(),
            log_file: Some(agent_log.clone()),
            attributed_log_path: Some(output_log.clone()),
            stderr_log_file: Some(stderr_raw.clone()),
            instance_name: "dual".to_string(),
            pipe_stdin: false,
        };
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&s).expect("spawn fake_agent");
        assert!(
            backend.log_capture(&proc).is_some(),
            "a freshly spawned, captured handle has a live log_capture"
        );

        // Wait for at least one heartbeat on EACH stream to land in its raw
        // file AND the attributed capture. A generous 10s deadline (rather
        // than 5s) — this test spawns a real process AND drives a
        // background tailer thread under whatever CI/parallel-test load is
        // in effect; the poll is deadline-based (never a fixed sleep), so a
        // slower runner just takes longer, not a false failure.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let legacy = std::fs::read_to_string(&agent_log).unwrap_or_default();
            let stderr = std::fs::read_to_string(&stderr_raw).unwrap_or_default();
            let attributed = std::fs::read_to_string(&output_log).unwrap_or_default();
            let raw_has_both = legacy.lines().any(|l| l.starts_with("heartbeat "))
                && stderr.lines().any(|l| l.starts_with("stderr-heartbeat "));
            let attributed_has_both = attributed.contains("\"stream\":\"agent-out\"")
                && attributed.contains("\"stream\":\"agent-err\"");
            if raw_has_both && attributed_has_both {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "never observed both streams in both captures; legacy={legacy:?} stderr={stderr:?} attributed={attributed:?}"
            );
            sleep(Duration::from_millis(20));
        }

        // The legacy log NEVER carries stderr content (the deliberate
        // change this test's rename documents) — even after the deadline
        // loop above already confirmed BOTH streams are captured
        // elsewhere, re-affirm this file specifically stays stdout-only.
        let legacy = std::fs::read_to_string(&agent_log).unwrap();
        assert!(
            !legacy.lines().any(|l| l.starts_with("stderr-heartbeat ")),
            "agent.log must never carry stderr content: {legacy:?}"
        );

        // The attributed capture parses as well-formed LogLine JSON-Lines,
        // each carrying exactly the expected attribution.
        let attributed = std::fs::read_to_string(&output_log).unwrap();
        for l in attributed.lines() {
            let parsed: LogLine = serde_json::from_str(l).expect("well-formed LogLine JSON");
            assert!(matches!(
                parsed.stream,
                LogStream::AgentOut | LogStream::AgentErr
            ));
            assert_eq!(parsed.instance, "dual");
        }

        let _ = backend.stop(&mut proc, Duration::from_millis(200));
    }

    #[test]
    fn adopted_handle_has_no_log_capture() {
        // Task 3: an adopted handle gets NO capture pipeline (mirrors
        // has_stdin_is_true_when_spawned_and_false_once_adopted's precedent
        // for stdin) — not a functional gap for reading/following (AC-H),
        // only for the live-write side this handle would need to originate
        // new capture from.
        let backend = UnixBackend::new();
        let mut proc = backend.spawn(&spec("sleep", &["30"])).expect("spawn sleep");
        assert!(
            backend.log_capture(&proc).is_none(),
            "spec() opts out of capture"
        );
        let _ = backend.stop(&mut proc, Duration::from_secs(2));

        // Spawn WITH capture this time, then adopt — the adopted copy must
        // have no log_capture even though the ORIGINAL did.
        let dir = tempfile::tempdir().unwrap();
        let s = SpawnSpec {
            exec: "sleep".to_string(),
            args: vec!["30".to_string()],
            env: BTreeMap::new(),
            working_dir: dir.path().to_path_buf(),
            log_file: Some(dir.path().join("agent.log")),
            attributed_log_path: Some(dir.path().join("output.log")),
            stderr_log_file: Some(dir.path().join("stderr.raw")),
            instance_name: "svc".to_string(),
            pipe_stdin: false,
        };
        let mut original = backend.spawn(&s).expect("spawn sleep with capture");
        assert!(backend.log_capture(&original).is_some());
        let fp = backend.fingerprint(&original);
        let adopter = UnixBackend::new();
        let adopted = adopter
            .adopt(&fp)
            .expect("adopt call ok")
            .expect("a live matching process must be adopted");
        assert!(
            adopter.log_capture(&adopted).is_none(),
            "an adopted handle has no recoverable capture pipeline"
        );
        let _ = backend.stop(&mut original, Duration::from_secs(2));
    }
}
