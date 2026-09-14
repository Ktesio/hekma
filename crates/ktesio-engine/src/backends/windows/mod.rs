//! The Windows [`ProcessBackend`] (spine AD-4) — one Job Object per instance.
//!
//! Each Agent Instance is spawned and then assigned to its OWN Job Object
//! configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Stopping terminates the
//! whole job with `TerminateJobObject`, which kills EVERY process in the job —
//! the parent agent and any child processes it spawned — the Windows equivalent
//! of the Unix process-group kill behind AC3 "no process of the instance
//! survives". Closing the job handle (on drop) also kills the tree, so a dropped
//! handle never leaks processes.
//!
//! This module is the allowlisted home for OS-conditional code (it is
//! `#[cfg(windows)]`-gated at its `mod` declaration in `backends/mod.rs`). It
//! uses raw `windows-sys` Job-Object FFI.
//!
//! ## Assign-after-spawn race (documented `[ASSUMPTION]`)
//!
//! `std::process::Command` does not expose the child's main-thread handle, so a
//! `CREATE_SUSPENDED` + resume dance is not possible with std alone. We instead
//! spawn the child and assign it to the job IMMEDIATELY (before it does
//! meaningful work). Any descendant the child spawns AFTER assignment is in the
//! job and dies with it; the sub-millisecond window before assignment is
//! acceptable for the runner's supervised agents (and the test agent sleeps
//! before spawning children, so assignment always wins). `TerminateJobObject`
//! plus kill-on-close guarantee the parent + all post-assignment descendants
//! die. This mirrors how established Job-Object supervisors handle the std
//! limitation.
//!
//! ## Graceful shutdown on Windows (documented `[ASSUMPTION]`)
//!
//! A console agent has no portable "please shut down" signal equivalent to
//! SIGTERM that std can deliver to an arbitrary child. This backend therefore
//! implements the graceful step as "give the process the window to exit on its
//! own, then terminate the job": it waits up to `graceful_window` for the
//! process to exit, and if it has not, escalates to `TerminateJobObject`
//! (`forced == true`), then CONFIRMS death bounded to
//! [`crate::ports::KILL_CONFIRM_TIMEOUT`] (fix pass, review of #80 follow-up
//! — the CRITICAL finding: a thread stuck in kernel-mode I/O is not
//! terminated until it returns to user mode, so termination can be sent
//! successfully yet the process still not actually exit for a while — see
//! that constant's docs). If the process exits within the graceful window,
//! the stop is graceful (`forced == false`). Richer graceful mechanisms (a
//! `CTRL_BREAK_EVENT` to the process group, or an adapter-specific shutdown
//! request) are a later refinement; the no-survivor guarantee is unchanged.
//!
//! ## Cooperative best-effort pause/resume (documented `[ASSUMPTION]`, AD-4)
//!
//! Windows has NO clean GUARANTEED whole-process suspend from `std`: the closest
//! primitives (`NtSuspendProcess`, or per-thread `SuspendThread` enumeration) are
//! undocumented / brittle, and `std::process::Command` does not even expose the
//! child's thread handles. Per AD-4 the honest Windows pause is therefore
//! **adapter-cooperative only** — never an undocumented suspend API. So
//! [`WindowsBackend::pause`] / [`WindowsBackend::resume`] succeed WITHOUT a hard
//! suspension: they are no-ops that report success. This is honest because the
//! engine only ever calls the backend pause/resume on the GUARANTEED dispatch
//! path; on Windows the mock/manifest declare pause `best-effort`, which the
//! SUPERVISOR handles by transitioning state AND emitting a VISIBLE best-effort
//! qualifier (a `pause-best-effort` transition cause + a CLI stderr note) — the
//! qualifier, not a silent fake in the backend, is what makes it "surfaced not
//! silent". These methods are BEHAVIOR-verified only on the `windows-latest` CI
//! matrix; on Unix hosts they are compile-checked only.
//!
//! ## Start-time fingerprint + orphan adoption (story 1-6, spine AD-5)
//!
//! [`WindowsBackend::fingerprint`] reads the process CREATION TIME via the
//! documented `GetProcessTimes` (a `FILETIME`, folded to a u64 of 100ns ticks) —
//! stable per process, different across a PID reuse. [`WindowsBackend::adopt`]
//! re-opens a live pid with `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION |
//! PROCESS_TERMINATE)` and compares its creation time to the recorded
//! fingerprint (the PID-reuse guard); a match yields an ADOPTED handle that holds
//! the process HANDLE (no Job — the process is already running and may already be
//! in one), so a subsequent `stop` uses `TerminateProcess` on that handle. No
//! undocumented API. Behavior-verified on the `windows-latest` CI leg;
//! compile-checked on Unix.

use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, TerminateProcess, WaitForSingleObject,
    CREATE_NEW_PROCESS_GROUP, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};

use crate::ports::{
    spawn_output_capture, write_stdin_bounded, BackendError, LogCapture, ProcessBackend,
    ProcessFingerprint, ProcessStatus, SecretError, SpawnSpec, StdinState, StopOutcome,
    KILL_CONFIRM_TIMEOUT, STDIN_WRITE_TIMEOUT,
};

/// `STILL_ACTIVE` (259): the exit code a process reports while still running.
const STILL_ACTIVE: u32 = 259;

/// How often the graceful-stop wait polls for the process to exit.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// AI-14 (loop 2): how many times the spawn path attempts the creation-time
/// read before failing closed, and how long it waits between attempts — enough
/// to absorb the spawn-race window without masking a genuine platform outage.
const START_TIME_READ_ATTEMPTS: usize = 3;
const START_TIME_READ_RETRY_DELAY: Duration = Duration::from_millis(10);

/// A running process on Windows.
///
/// For a FRESHLY SPAWNED process, `child` is `Some` and `job` owns the process
/// tree (kill-on-close). For an ADOPTED process (story 1-6, re-acquired on engine
/// start), `child` is `None`, `job` is null, and `adopted` holds a process HANDLE
/// opened via `OpenProcess` (for liveness + `TerminateProcess`) — this engine is
/// not the parent, so it holds no reap-able [`Child`] and did not create a job.
/// Dropping either form releases its OS handles; a spawned handle also kills the
/// tree via the job's kill-on-close.
pub struct WindowsProcess {
    /// The owned child handle if THIS engine spawned the process (drives
    /// waits/exit-code). `None` for an adopted process (not our child).
    child: Option<Child>,
    /// The Job Object handle (owns the process tree; kill-on-close configured)
    /// for a spawned process; null for an adopted one.
    job: HANDLE,
    /// The opened process HANDLE for an ADOPTED process (liveness +
    /// TerminateProcess); null for a spawned one (which uses its Child/job).
    adopted: HANDLE,
    /// The child pid, cached for diagnostics and the 1-6 adoption fingerprint.
    pid: u32,
    /// The verified process creation-time token (spine AD-5) — the write-ahead
    /// fingerprint's PID-reuse guard. AI-14: captured + VERIFIED at spawn (a
    /// failed read FAILS the spawn, so the engine's own paths never record the
    /// `0` sentinel) or at adoption (`adopt` returns `None` when it cannot read
    /// one). Windows adopted-handle liveness uses the opened process HANDLE (no
    /// start-time re-check needed — parity with the Unix backend's spawned-path
    /// reasoning), so this field exists to make [`WindowsBackend::fingerprint`]
    /// return exactly the token the spawn/adopt verified, never a second,
    /// separately-fallible read that could silently produce `0`.
    start_time: u64,
    /// The child's stdin channel state (story 4.1, spine AD-12; fix pass —
    /// CRITICAL/HIGH findings, review of #79). `Live` only for a FRESHLY
    /// SPAWNED process whose declared `Capability::Interaction` was
    /// `Guaranteed`/`BestEffort` on this OS (`SpawnSpec::pipe_stdin`);
    /// `NoPipe` for an ADOPTED process (a pipe handle cannot be recovered
    /// from a bare PID — no undocumented API; parity with the Unix backend
    /// and this module's own pause `[ASSUMPTION]` precedent) or a freshly
    /// spawned one that was never piped (interaction `Unsupported`);
    /// `TimedOut` once a bounded write on this handle has exceeded
    /// [`STDIN_WRITE_TIMEOUT`] and can never be safely retried. `send_input`
    /// on anything but a `Live` state must therefore fail honestly
    /// (`EngineError::InteractionUnavailable` /
    /// `EngineError::InteractionTimedOut`), never silently succeed.
    stdin: StdinState,
    /// The output-capture pipeline handle (story 4-2, AD-12; fix pass,
    /// review of #80), if this handle has one. `Some` for a FRESHLY SPAWNED
    /// handle whenever the caller gave us somewhere to capture; `None` for
    /// an ADOPTED process — no live tailer thread survives the engine
    /// process that spawned it (parity with `stdin`'s `NoPipe`-on-
    /// adoption). Not a functional gap for `kt agent logs`/`--follow`
    /// (AC-H): reading only needs the crash-immune raw FILES, which the
    /// agent process itself keeps writing to directly (never through any
    /// engine-held handle) for as long as it lives.
    log_capture: Option<LogCapture>,
}

// The raw Job / process HANDLEs are owned OS resources this struct is solely
// responsible for; it is safe to move across threads (tokio's blocking pool).
unsafe impl Send for WindowsProcess {}

impl Drop for WindowsProcess {
    /// Fix pass (review of #80): ALSO signals the output-capture pipeline's
    /// background tailer thread to stop (one final catch-up pass, then
    /// exit) — unconditionally, mirroring the Unix backend's identical
    /// addition. Purely local bookkeeping; the agent process's crash
    /// resilience comes entirely from the raw capture files being direct,
    /// engine-independent OS redirects, never from this signal.
    fn drop(&mut self) {
        if let Some(capture) = &self.log_capture {
            capture.signal_stop();
        }
        // Spawned: closing the job handle kills the tree (kill-on-close), then
        // release it. Adopted: SIGKILL-equivalent is not applied on drop for a
        // process we merely re-opened (parity with Unix would kill it; but on
        // Windows an adopted process has no job, and the cross-lifetime handle is
        // dropped at engine shutdown — we terminate it in `stop`, and on drop we
        // only release the opened handle so we do not leak it). Best-effort.
        if !self.job.is_null() {
            unsafe {
                CloseHandle(self.job);
            }
            self.job = std::ptr::null_mut();
        }
        if !self.adopted.is_null() {
            unsafe {
                CloseHandle(self.adopted);
            }
            self.adopted = std::ptr::null_mut();
        }
    }
}

/// The Windows process backend (AD-4).
///
/// Stateless — each running process is owned by its [`WindowsProcess`] handle.
/// Constructed via [`crate::backends::current`].
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowsBackend;

impl WindowsBackend {
    /// Construct the backend.
    pub fn new() -> Self {
        WindowsBackend
    }
}

impl ProcessBackend for WindowsBackend {
    type Handle = WindowsProcess;

    fn spawn(&self, spec: &SpawnSpec) -> Result<Self::Handle, BackendError> {
        // Create the Job Object first and configure kill-on-close so that even a
        // dropped handle (or a crash of the engine) tears down the tree.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(BackendError::Spawn {
                exec: spec.exec.clone(),
                detail: format!("CreateJobObjectW failed (os error {})", last_error()),
            });
        }
        // Wrap the job handle in a guard so any early return closes it.
        let job_guard = JobGuard { job };

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(BackendError::Spawn {
                exec: spec.exec.clone(),
                detail: format!("SetInformationJobObject failed (os error {})", last_error()),
            });
        }

        let mut command = Command::new(&spec.exec);
        command.args(&spec.args);
        command.current_dir(&spec.working_dir);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        // Story 4-2 (AD-12, AC-E), fix pass (review of #80): stdout/stderr
        // capture is UNCONDITIONAL and capability-independent — each stream
        // is redirected DIRECTLY to its OWN regular file (crash-immune, NOT
        // a pipe) whenever the caller gave us somewhere to write all THREE
        // capture destinations (every PRODUCTION spawn does; the supervisor
        // always computes `log_file`/`stderr_log_file`/`attributed_log_path`
        // together from the SAME Registry path authority). Mirrors the Unix
        // backend's identical branch — see its comment for the full
        // rationale (including why `None`/`None`/`None` is a narrow
        // test-fixture convenience, not a capability gate).
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
        // IS the fail-fast proof); the attributed path is validated then
        // dropped (whichever of the background tailer thread or an inline
        // `send_engine_line` call reopens it per-append). Mirrors the Unix
        // backend's identical logic.
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
        // DIRECT, crash-immune redirects (never `Stdio::piped()`): the
        // agent's `write()` to either stream succeeds or fails based ONLY
        // on this regular file, never on whether the engine process is even
        // still alive to read anything.
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
        // lifetime. Mirrors the Unix backend's identical branch.
        command.stdin(if spec.pipe_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        // A new process group isolates console signals (so a stray Ctrl-C to the
        // engine's console does not hit the agent). We do NOT create the child
        // suspended: std does not expose the main-thread handle needed to resume
        // it, so instead we spawn and assign to the job IMMEDIATELY (see the
        // module docs on the sub-millisecond assign-after-spawn window).
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);

        let mut child = command.spawn().map_err(|e| BackendError::Spawn {
            exec: spec.exec.clone(),
            detail: e.to_string(),
        })?;
        let pid = child.id();

        // Assign the child to the job immediately (before it does meaningful
        // work). From here, the child and every descendant it spawns are in the
        // job and die with a TerminateJobObject / job-handle close (AC3).
        let child_handle = child.as_raw_handle() as HANDLE;
        let assigned = unsafe { AssignProcessToJobObject(job, child_handle) };
        if assigned == 0 {
            let detail = format!(
                "AssignProcessToJobObject failed (os error {})",
                last_error()
            );
            // Kill the child we just created so nothing leaks, then fail.
            unsafe {
                TerminateJobObject(job, 1);
            }
            return Err(BackendError::Spawn {
                exec: spec.exec.clone(),
                detail,
            });
        }

        // AI-14: verify the child's creation-time token NOW — a failed read
        // FAILS the spawn (fail closed) instead of recording the
        // `start_time = 0` sentinel, which would silently downgrade every later
        // orphan adoption to a pid-only match. Placed BEFORE the job guard
        // release: on failure the guard's Drop closes the job, and
        // KILL_ON_JOB_CLOSE reaps the just-spawned child — no orphan, no
        // unrecorded process. Loop 2: a read that fails because the pid is
        // ALREADY GONE is surfaced as an instant agent exit (not a platform
        // failure), and the few-attempt retry absorbs the spawn-race window.
        let start_time = {
            let mut token = None;
            for attempt in 0..START_TIME_READ_ATTEMPTS {
                match process_start_time(pid) {
                    Some(t) => {
                        token = Some(t);
                        break;
                    }
                    None if attempt + 1 < START_TIME_READ_ATTEMPTS => {
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
                    let exited_instantly = matches!(child.try_wait(), Ok(Some(_)));
                    let detail = if exited_instantly {
                        format!(
                            "the spawned agent exited immediately, before its process \
                             creation time could be read for pid {pid} — the agent failed \
                             at startup (check its logs); this is not a platform \
                             start-time-source failure"
                        )
                    } else {
                        format!(
                            "could not read the process creation time for the spawned pid \
                             {pid} — no usable process start-time source on this platform; \
                             cannot guarantee pid-reuse safety, so refusing to record a \
                             start-time-less fingerprint (sentinel 0): the write-ahead \
                             spawn record needs the real token for orphan adoption"
                        )
                    };
                    return Err(BackendError::Spawn {
                        exec: spec.exec.clone(),
                        detail,
                    });
                }
            }
        };
        // Assignment succeeded — hand the job handle to the process struct.
        let job = job_guard.into_inner();
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
        // them by path on every poll, which is what makes this crash-immune.
        let log_capture = capture.map(|(stdout_raw_path, stderr_raw_path, attributed_log_path)| {
            spawn_output_capture(
                stdout_raw_path,
                stderr_raw_path,
                attributed_log_path,
                spec.instance_name.clone(),
            )
        });
        Ok(WindowsProcess {
            child: Some(child),
            job,
            adopted: std::ptr::null_mut(),
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
        // Already exited? Reap and report a graceful stop.
        if handle.reap_if_exited()?.is_exited() {
            return Ok(StopOutcome { forced: false });
        }

        // Graceful step: give the process the window to exit on its own. (See
        // module docs on Windows graceful semantics.)
        let deadline = Instant::now() + graceful_window;
        loop {
            if handle.reap_if_exited()?.is_exited() {
                return Ok(StopOutcome { forced: false });
            }
            if Instant::now() >= deadline {
                break;
            }
            sleep(STOP_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
        }

        // Escalate. Spawned: terminate the whole job (kills parent + descendants).
        // Adopted (no job, no child): terminate the opened process HANDLE with
        // TerminateProcess.
        if !handle.job.is_null() {
            let ok = unsafe { TerminateJobObject(handle.job, 1) };
            if ok == 0 {
                return Err(BackendError::Control {
                    op: "terminate",
                    detail: format!("TerminateJobObject failed (os error {})", last_error()),
                });
            }
        } else if !handle.adopted.is_null() {
            let ok = unsafe { TerminateProcess(handle.adopted, 1) };
            if ok == 0 {
                return Err(BackendError::Control {
                    op: "terminate",
                    detail: format!("TerminateProcess failed (os error {})", last_error()),
                });
            }
        }
        // CONFIRM death — bounded to KILL_CONFIRM_TIMEOUT (fix pass, review
        // of #80 follow-up — the CRITICAL finding; see that constant's docs
        // for the full mechanism, which applies identically on Windows: a
        // thread stuck in kernel-mode I/O is not terminated until it returns
        // to user mode, so `TerminateJobObject`/`TerminateProcess` can be
        // sent successfully yet the process still does not actually exit
        // for a while). A single unified polling loop via `reap_if_exited`
        // (NON-BLOCKING on both the spawned — `try_wait` +
        // `WaitForSingleObject(_, 0)` — and adopted branches) replaces the
        // OLD unbounded `child.wait()` (spawned) / a fire-and-forget
        // `WaitForSingleObject(_, 5000)` whose result was discarded (adopted
        // — it silently returned `Ok` even if the process was still alive
        // after its own 5s wait).
        confirm_death(handle, KILL_CONFIRM_TIMEOUT)?;
        Ok(StopOutcome { forced: true })
    }

    fn poll(&self, handle: &mut Self::Handle) -> Result<ProcessStatus, BackendError> {
        handle.reap_if_exited()
    }

    fn pause(&self, handle: &mut Self::Handle) -> Result<(), BackendError> {
        // Cooperative best-effort pause on Windows (AD-4): NO guaranteed
        // whole-process suspend is available from std, and we do NOT reach for an
        // undocumented API (see the module `[ASSUMPTION]` block). Succeed without
        // a hard suspension — the VISIBLE best-effort qualifier the supervisor/CLI
        // emit (never a silent fake here) carries the honesty. We still touch the
        // liveness guard for parity with the Unix body and to reap a gone child.
        let _ = handle.reap_if_exited()?;
        Ok(())
    }

    fn resume(&self, handle: &mut Self::Handle) -> Result<(), BackendError> {
        // Cooperative best-effort resume on Windows — the counterpart of `pause`.
        let _ = handle.reap_if_exited()?;
        Ok(())
    }

    fn pid(&self, handle: &Self::Handle) -> u32 {
        handle.pid
    }

    fn fingerprint(&self, handle: &Self::Handle) -> ProcessFingerprint {
        // The handle's OWN creation-time token — VERIFIED at spawn (AI-14: the
        // spawn FAILS when the read fails, so a spawned handle always carries a
        // real, non-zero token) or at adoption (`adopt` returns `None` when it
        // cannot read one). No second, separately-fallible read that could
        // silently produce a `0` sentinel on the write-ahead record.
        ProcessFingerprint::new(handle.pid, handle.start_time)
    }

    fn adopt(
        &self,
        fingerprint: &ProcessFingerprint,
    ) -> Result<Option<Self::Handle>, BackendError> {
        // Open the pid for query + terminate. A gone pid → OpenProcess fails →
        // Ok(None). Then compare the CURRENT creation time to the recorded one
        // (the PID-reuse guard, AD-5): a mismatch → a different process → Ok(None).
        let h = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                0,
                fingerprint.pid,
            )
        };
        if h.is_null() {
            return Ok(None);
        }
        let live_start = match process_start_time(fingerprint.pid) {
            Some(t) => t,
            None => {
                unsafe {
                    CloseHandle(h);
                }
                return Ok(None);
            }
        };
        if live_start != fingerprint.start_time {
            // PID reused by a different process — do NOT adopt.
            unsafe {
                CloseHandle(h);
            }
            return Ok(None);
        }
        // Same process. Hold the opened handle for liveness + TerminateProcess
        // (no Job — the process is already running and may be in one already).
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
        // directly, for as long as it lives.
        Ok(Some(WindowsProcess {
            child: None,
            job: std::ptr::null_mut(),
            adopted: h,
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
        // `write_stdin_bounded`'s docs. Identical to the Unix backend's body
        // (this file and `backends/unix/mod.rs` intentionally share ONE
        // implementation via this call, rather than two separate OS timeout
        // implementations, since the mechanism has no OS-specific part: a
        // `ChildStdin` write is portable `std` on both).
        write_stdin_bounded(&mut handle.stdin, data, STDIN_WRITE_TIMEOUT)
    }

    fn log_capture(&self, handle: &Self::Handle) -> Option<LogCapture> {
        handle.log_capture.clone()
    }
}

impl WindowsProcess {
    /// Non-blocking: reap the child if it has exited, returning its status.
    ///
    /// SPAWNED (`child: Some`): `Child::try_wait` (authoritative), double-checked
    /// via the raw handle. ADOPTED (`child: None`, story 1-6): this engine is not
    /// the parent, so liveness is `WaitForSingleObject(adopted, 0)` +
    /// `GetExitCodeProcess` on the opened handle; a gone process reports its exit
    /// code if still readable, else `Exited { code: None }`.
    fn reap_if_exited(&mut self) -> Result<ProcessStatus, BackendError> {
        match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => Ok(ProcessStatus::Exited {
                    code: status.code(),
                }),
                Ok(None) => {
                    // Double-check via the raw handle (defensive; try_wait is
                    // authoritative but this keeps parity with the Unix poll).
                    let h = child.as_raw_handle() as HANDLE;
                    let waited = unsafe { WaitForSingleObject(h, 0) };
                    if waited == WAIT_OBJECT_0 {
                        let mut code: u32 = 0;
                        let ok = unsafe { GetExitCodeProcess(h, &mut code) };
                        if ok != 0 && code != STILL_ACTIVE {
                            return Ok(ProcessStatus::Exited {
                                code: Some(code as i32),
                            });
                        }
                    }
                    Ok(ProcessStatus::Alive)
                }
                Err(e) => Err(BackendError::Control {
                    op: "wait",
                    detail: e.to_string(),
                }),
            },
            // Adopted: liveness via the opened process handle.
            None => {
                if self.adopted.is_null() {
                    // No handle at all — treat as gone (defensive; not normally
                    // reachable, an adopted handle always opens a process handle).
                    return Ok(ProcessStatus::Exited { code: None });
                }
                let waited = unsafe { WaitForSingleObject(self.adopted, 0) };
                if waited == WAIT_OBJECT_0 {
                    let mut code: u32 = 0;
                    let ok = unsafe { GetExitCodeProcess(self.adopted, &mut code) };
                    if ok != 0 && code != STILL_ACTIVE {
                        return Ok(ProcessStatus::Exited {
                            code: Some(code as i32),
                        });
                    }
                    return Ok(ProcessStatus::Exited { code: None });
                }
                Ok(ProcessStatus::Alive)
            }
        }
    }
}

/// Read a process's creation time via `GetProcessTimes`, folded to a u64 of
/// 100ns ticks — stable per process, different across a PID reuse (spine AD-5).
/// Opens a short-lived query handle by pid. Returns `None` if the process cannot
/// be opened/queried (gone, or insufficient rights). No undocumented API.
fn process_start_time(pid: u32) -> Option<u64> {
    // PROCESS_QUERY_LIMITED_INFORMATION suffices for GetProcessTimes and is the
    // least-privileged right that works across integrity levels.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    let ok = unsafe { GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user) };
    unsafe {
        CloseHandle(h);
    }
    if ok == 0 {
        return None;
    }
    let ticks = ((creation.dwHighDateTime as u64) << 32) | (creation.dwLowDateTime as u64);
    if ticks == 0 {
        return None;
    }
    Some(ticks)
}

/// A guard that closes a Job handle unless [`JobGuard::into_inner`] is called.
///
/// Ensures an early return between `CreateJobObjectW` and handing the handle to
/// the [`WindowsProcess`] does not leak the job.
struct JobGuard {
    job: HANDLE,
}

impl JobGuard {
    /// Release the handle from the guard (the caller now owns it).
    fn into_inner(self) -> HANDLE {
        let job = self.job;
        std::mem::forget(self);
        job
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        if !self.job.is_null() {
            unsafe {
                CloseHandle(self.job);
            }
        }
    }
}

/// The last OS error code (`GetLastError`) as a `u32`, for diagnostics.
fn last_error() -> u32 {
    // SAFETY: GetLastError is always safe to call and has no preconditions.
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}

/// Poll `handle` (via the NON-BLOCKING [`WindowsProcess::reap_if_exited`])
/// until it reports exited, or `timeout` elapses — the bounded
/// death-confirmation primitive [`WindowsBackend::stop`]'s escalation phase
/// uses (fix pass, review of #80 follow-up — the CRITICAL finding). Mirrors
/// the Unix backend's identical `confirm_death` helper.
///
/// `timeout` is a PARAMETER (never hardcoded in this function) so it stays
/// directly unit-testable with a SHORT duration for fast, deterministic
/// coverage of the bound-enforcement logic itself — mirrors
/// [`write_stdin_bounded`]'s existing "timeout as a parameter, tested
/// directly with a short value" precedent (story 4.1 fix pass); production
/// calls this with [`KILL_CONFIRM_TIMEOUT`]. Returns `Ok(())` once confirmed
/// dead; [`BackendError::StopUnconfirmed`] if `timeout` elapses first
/// (naming the ACTUAL `timeout` passed); any other [`BackendError`] from
/// `reap_if_exited` propagates unchanged.
fn confirm_death(handle: &mut WindowsProcess, timeout: Duration) -> Result<(), BackendError> {
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

/// Check the engine secrets file's permissions on Windows (story 2-4 AC6, spine
/// AD-10/AD-4) — the OS-specific INSPECTION confined to `backends/`.
///
/// DECISION (Assumption 7, option B — documented portable skip): Unix mode bits do
/// not exist on Windows, and a faithful DACL inspection (option A) needs `windows`
/// ACL FFI that is over-scope for v1's tiny-secrets budget. So this does NOT
/// attempt a Unix-style refusal: it returns `Ok(())` and relies on the DEFAULT
/// per-user profile ACLs — the state dir lives under the user's profile
/// (`%APPDATA%`/`%LOCALAPPDATA%` via the `directories` crate), which is
/// per-user-protected by Windows by default. This is an HONEST boundary (documented
/// in `docs/architecture.md`, NFR-6): it avoids a FALSE PASS masquerading as a
/// Unix-grade check AND avoids a hard failure that would make secrets UNUSABLE on
/// Windows. A future ACL-checking resolver can strengthen this behind the same
/// port without a schema/API change. The `_path` is accepted for signature
/// symmetry with the Unix backend.
pub fn check_secrets_file_permissions(_path: &std::path::Path) -> Result<(), SecretError> {
    // Portable skip (option B): Windows relies on default per-user profile ACLs.
    // Never a false pass framed as a Unix-grade check; never a hard failure.
    Ok(())
}

/// "Copy" an existing target file's permissions onto the freshly written `temp`
/// before the atomic rename (story 11-2 review-1, patch 3). Windows carries no
/// Unix mode bits — the SAME documented portable posture as
/// [`check_secrets_file_permissions`] above: no Unix-style check, and the fresh
/// file takes the creating process's DEFAULT per-user-profile ACLs (the state
/// dir lives under the user's profile, which Windows protects per-user by
/// default). Always `Ok`; never a false pass framed as a Unix-grade copy. The
/// `_target` is accepted for signature symmetry with the Unix backend.
pub fn preserve_target_mode(
    _target: &std::path::Path,
    _temp: &std::path::Path,
) -> std::io::Result<()> {
    Ok(())
}

/// Rename the temp over the atomic-write target (story 11-2 review-1, patch 4).
/// Windows' `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` FAILS with a sharing
/// violation while another process holds the target open WITHOUT
/// `FILE_SHARE_DELETE` (editors, AV scanners, indexers). One short backoff +
/// a single retry rides out the transient window; a PERSISTENT hold surfaces
/// the honest error — the atomic write fails with the target untouched,
/// where the old in-place `fs::write` would have silently overwritten (or
/// half-written) the bytes under the reader.
pub fn rename_over_target(temp: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    if let Ok(()) = std::fs::rename(temp, target) {
        return Ok(());
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    std::fs::rename(temp, target)
}

#[cfg(test)]
mod tests {
    //! AI-14 (story 11-5) — the fail-closed spawn arm's HOSTED tests. These run
    //! only where the Windows backend runs (the `windows-latest` matrix leg of
    //! the CI `test` job; on a Unix host this whole module is compile-checked
    //! only, via `cargo check --target x86_64-pc-windows-gnu`). `cfg(test)` plus
    //! OS cfg are allowed HERE: this file is inside the boundary gate's
    //! `crates/ktesio-engine/src/backends/` allowlist home. Together the two
    //! tests close the 11-1 defer: the REAL-child arm proves the production
    //! spawn's creation-time read works and yields a verified non-zero token,
    //! and the failed-read path is pinned at the unit seam
    //! (`process_start_time`) where it is observable without an injected fault.

    use super::*;
    use std::collections::BTreeMap;

    /// A `SpawnSpec` for a real child, mirroring the Unix backend test
    /// module's helper (the capture trio `None` = the narrow don't-care
    /// fixture; no stdin pipe — this helper never writes to the child).
    fn spec(exec: &str, args: &[&str]) -> SpawnSpec {
        SpawnSpec {
            exec: exec.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
            working_dir: std::env::temp_dir(),
            log_file: None,
            attributed_log_path: None,
            stderr_log_file: None,
            instance_name: "test".to_string(),
            pipe_stdin: false,
        }
    }

    #[test]
    fn spawn_of_a_real_child_verifies_a_nonzero_creation_time_and_stops_clean() {
        // The POSITIVE half of the AI-14 fail-closed spawn arm. The production
        // `spawn` reads the child's creation time right after the job
        // assignment and FAILS the spawn when the read cannot be verified — so
        // a real child's spawn SUCCEEDING is itself the proof that the read
        // works, and the resulting fingerprint must carry a REAL (non-zero)
        // token, never the `start_time = 0` sentinel AI-14 forbids. Uses the
        // conformance `fake_agent` (an engine dev-dependency, off the shipping
        // graph), lingering long enough to be polled and stopped
        // deterministically.
        let backend = WindowsBackend::new();
        let agent = ktesio_conformance::fake_agent_bin();
        let mut handle = backend
            .spawn(&spec(&agent.to_string_lossy(), &["--linger-ms", "600000"]))
            .expect("spawn of a real child must succeed (the creation-time read must work)");
        let fp = backend.fingerprint(&handle);
        assert_eq!(fp.pid, backend.pid(&handle));
        assert!(
            fp.start_time != 0,
            "a spawned handle must carry the VERIFIED creation-time token, never the 0 \
             sentinel (AI-14 fail-closed contract)"
        );
        // Alive now; the later stop terminates the whole job (kill-on-close).
        assert_eq!(backend.poll(&mut handle).unwrap(), ProcessStatus::Alive);
        let outcome = backend
            .stop(&mut handle, Duration::from_secs(5))
            .expect("stop the lingering agent");
        assert!(
            outcome.forced,
            "a lingering agent needs the forced escalation"
        );
        assert!(backend.poll(&mut handle).unwrap().is_exited());
    }

    #[test]
    fn start_time_read_fails_closed_for_an_absent_pid() {
        // The FAILED-READ path of the AI-14 arm, pinned at the unit seam
        // (`process_start_time`) where it is honestly observable: a pid that
        // cannot exist on Windows (pids are 4-aligned and live far below the
        // u32 ceiling) fails the OpenProcess query and returns None — exactly
        // the reading the spawn arm treats as "cannot verify" and FAILS THE
        // SPAWN on (after its bounded, spawn-race-absorbing retry), never
        // recording a `start_time = 0` fingerprint.
        assert_eq!(process_start_time(0xFFFF_FFFC), None);
    }
}
