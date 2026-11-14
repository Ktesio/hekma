//! The Shim server (story 24-1): one control connection at a time, the
//! child launched once through the existing per-OS process backend (the
//! group or Job, the pipes and log redirects are the backend's own code,
//! run inside the Shim), its exit buffered while no engine is connected.
//!
//! What the Shim never does: relaunch the child, write state.db, mint a
//! credential, or let a second engine take over a connected one (it
//! answers `busy`). It leaves on its own only by the two lingers: never
//! launched within [`super::SHIM_IDLE_LINGER`], or its child's exit
//! acknowledged (or unacknowledged for [`super::SHIM_EXIT_LINGER`]).

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::proto::{
    decode_bytes, ErrorCode, ExitRecord, Op, Outcome, Request, ShimFrame, ShimStatus, Version,
};
use super::secret;
use crate::ports::{ProcessBackend, ProcessStatus};

/// A connected stream (a Unix socket, later a Windows pipe).
pub trait ShimStream: Read + Write + Send {
    /// A second handle on the same connection (one reads, one writes).
    fn try_clone_stream(&self) -> std::io::Result<Box<dyn ShimStream>>;
    /// Bound a blocking read (`None` = no bound).
    fn set_read_bound(&self, bound: Option<Duration>) -> std::io::Result<()>;
}

/// A listening endpoint.
pub trait ShimListener: Send {
    /// One pending connection, without blocking: the stream and whether its
    /// peer is the Shim's owner. `Ok(None)` when none is waiting.
    fn accept_pending(&self) -> std::io::Result<Option<(Box<dyn ShimStream>, bool)>>;
}

/// What the Shim expects of its controller.
#[derive(Clone)]
pub struct ShimConfig {
    /// `SHA-256(secret)` — the Shim never holds the secret itself.
    pub expected_secret_hash: [u8; 32],
    /// How long to wait for a launch, and to keep an unacknowledged exit
    /// (the defaults are the two lingers; tests shorten them).
    pub idle_linger: Duration,
    pub exit_linger: Duration,
}

impl ShimConfig {
    /// The production configuration for `expected_secret_hash`.
    pub fn new(expected_secret_hash: [u8; 32]) -> Self {
        ShimConfig {
            expected_secret_hash,
            idle_linger: super::SHIM_IDLE_LINGER,
            exit_linger: super::SHIM_EXIT_LINGER,
        }
    }
}

/// Why [`serve`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShimExit {
    /// No launch came within the idle linger.
    NeverLaunched,
    /// The child exited and an engine acknowledged it.
    Acknowledged,
    /// The child exited and no engine acknowledged it within the linger.
    Unacknowledged,
}

struct Inner {
    backend: crate::backends::Backend,
    handle: Option<crate::backends::Handle>,
    status: ShimStatus,
    /// The connected controller's writer, when one is connected.
    controller: Option<Box<dyn ShimStream>>,
    exit_seen: Option<Instant>,
    acknowledged: bool,
    /// Story 22-4: what to compare the tree against, and when next.
    bypass: Option<super::proto::BypassConfig>,
    next_bypass: Option<Instant>,
}

impl Inner {
    /// Write one frame to the controller; a failed write drops it.
    fn send(&mut self, frame: &ShimFrame) {
        let failed = match self.controller.as_mut() {
            Some(writer) => writer
                .write_all(frame.line().as_bytes())
                .and_then(|()| writer.flush())
                .is_err(),
            None => false,
        };
        if failed {
            self.controller = None;
        }
    }

    /// Record the child's exit once, and push it to the controller.
    fn note_exit(&mut self, code: Option<i32>) {
        if self.status.exit.is_some() {
            return;
        }
        let exit = ExitRecord {
            code,
            at: crate::time::now_rfc3339(),
        };
        self.status.exit = Some(exit.clone());
        self.exit_seen = Some(Instant::now());
        self.send(&ShimFrame::Exit { exit });
    }

    /// Poll the child; record its exit when it exited.
    fn poll_child(&mut self) {
        if self.status.exit.is_some() {
            return;
        }
        let Some(handle) = self.handle.as_mut() else {
            return;
        };
        if let Ok(ProcessStatus::Exited { code }) = self.backend.poll(handle) {
            self.note_exit(code);
        }
    }

    /// Story 22-4: the child's pid and the list, when a check is due (and
    /// the next one is scheduled).
    fn bypass_due(&mut self) -> Option<(u32, Vec<crate::domain::bypass::ProviderHost>)> {
        let config = self.bypass.as_ref()?;
        let (pid, _) = self.status.child?;
        if self.status.exit.is_some() || self.next_bypass.is_some_and(|at| Instant::now() < at) {
            return None;
        }
        self.next_bypass = Some(Instant::now() + Duration::from_millis(config.interval_ms.max(1)));
        Some((pid, config.hosts.clone()))
    }

    /// Story 22-4: fold one check into the report; push it when it changed.
    fn apply_bypass(
        &mut self,
        result: Result<Vec<std::net::SocketAddr>, String>,
        hosts: &[crate::domain::bypass::ProviderHost],
    ) {
        let before = self.status.bypass.clone();
        let report = &mut self.status.bypass;
        match result {
            Ok(addrs) => {
                report.state = "active".to_string();
                report.reason = None;
                for host in crate::domain::bypass::matching_hosts(hosts, &addrs) {
                    if !report.hosts.contains(&host)
                        && report.hosts.len() < crate::domain::bypass::MAX_BYPASS_HOSTS_PER_RUN
                    {
                        report.hosts.push(host);
                    }
                }
                report.hosts.sort();
            }
            Err(reason) => {
                report.state = "unavailable".to_string();
                report.reason = Some(reason);
            }
        }
        if self.status.bypass != before {
            let report = self.status.bypass.clone();
            self.send(&ShimFrame::Bypass { report });
        }
    }

    fn dispatch(&mut self, op: Op) -> Outcome {
        match op {
            Op::Hello { .. } => Outcome::error(ErrorCode::WrongState),
            Op::Spawn { spec } => {
                if self.status.launched {
                    return Outcome::error(ErrorCode::WrongState);
                }
                let bypass = spec.bypass.clone().map(|config| *config);
                let Some(spec) = spec.spec() else {
                    return Outcome::error(ErrorCode::BadFrame);
                };
                let launched = self.backend.spawn(&spec);
                // The request's environment leaves memory here, launched or
                // not (it may hold the Run Key).
                drop(spec);
                self.status.env_dropped = true;
                match launched {
                    Ok(handle) => {
                        let print = self.backend.fingerprint(&handle);
                        self.status.launched = true;
                        self.status.child = Some((print.pid, print.start_time));
                        self.status.has_stdin = self.backend.has_stdin(&handle);
                        self.handle = Some(handle);
                        // Story 22-4: detection starts with the child.
                        self.status.bypass.state = "unreported".to_string();
                        self.next_bypass = bypass.as_ref().map(|_| Instant::now());
                        self.bypass = bypass;
                        Outcome::Spawned {
                            pid: print.pid,
                            start_time: print.start_time,
                        }
                    }
                    Err(_) => Outcome::error(ErrorCode::Backend),
                }
            }
            Op::Stop { graceful_ms } => {
                let Some(handle) = self.handle.as_mut() else {
                    return Outcome::error(ErrorCode::WrongState);
                };
                match self
                    .backend
                    .stop(handle, Duration::from_millis(graceful_ms))
                {
                    Ok(outcome) => {
                        self.poll_child();
                        Outcome::Stopped {
                            forced: outcome.forced,
                        }
                    }
                    Err(_) => Outcome::error(ErrorCode::Backend),
                }
            }
            Op::Pause | Op::Resume => {
                let pause = matches!(op, Op::Pause);
                let Some(handle) = self.handle.as_mut() else {
                    return Outcome::error(ErrorCode::WrongState);
                };
                let done = if pause {
                    self.backend.pause(handle)
                } else {
                    self.backend.resume(handle)
                };
                match done {
                    Ok(()) => Outcome::Done,
                    Err(_) => Outcome::error(ErrorCode::Backend),
                }
            }
            Op::Poll => {
                if !self.status.launched {
                    return Outcome::error(ErrorCode::WrongState);
                }
                self.poll_child();
                Outcome::Polled {
                    exit: self.status.exit.clone(),
                }
            }
            Op::WriteStdin { data } => {
                let Some(bytes) = decode_bytes(&data) else {
                    return Outcome::error(ErrorCode::BadFrame);
                };
                let Some(handle) = self.handle.as_mut() else {
                    return Outcome::error(ErrorCode::WrongState);
                };
                if self.status.exit.is_some() {
                    return Outcome::error(ErrorCode::ChildGone);
                }
                match self.backend.write_stdin(handle, &bytes) {
                    Ok(()) => Outcome::Done,
                    Err(_) => Outcome::error(ErrorCode::Backend),
                }
            }
            Op::Status => Outcome::Status {
                status: self.status.clone(),
            },
            Op::AckExit => {
                if self.status.exit.is_none() {
                    return Outcome::error(ErrorCode::WrongState);
                }
                self.acknowledged = true;
                Outcome::Done
            }
            Op::BypassHosts { bypass } => {
                // The whole set is replaced; a Shim checking for the first
                // time checks at once.
                if self.bypass.is_none() {
                    self.next_bypass = Some(Instant::now());
                }
                self.bypass = Some(bypass);
                Outcome::Done
            }
        }
    }
}

/// One line from `reader`, or `None` at the end of the stream.
fn read_line(reader: &mut impl BufRead) -> Option<String> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line),
    }
}

fn write_frame(stream: &mut dyn ShimStream, frame: &ShimFrame) {
    let _ = stream.write_all(frame.line().as_bytes());
    let _ = stream.flush();
}

/// Admit (or refuse) a new connection: the owner check, the single
/// controller, the secret. An admitted connection is served on its own
/// thread until it closes.
fn admit(
    inner: &Arc<Mutex<Inner>>,
    config: &ShimConfig,
    mut stream: Box<dyn ShimStream>,
    owner: bool,
) {
    // Whatever the peer sends first must arrive promptly.
    let _ = stream.set_read_bound(Some(Duration::from_secs(5)));
    let Ok(reader) = stream.try_clone_stream() else {
        return;
    };
    let mut reader = BufReader::new(reader);
    let Some(line) = read_line(&mut reader) else {
        return;
    };
    let request: Option<Request> = serde_json::from_str(line.trim()).ok();
    let id = request.as_ref().map(|r| r.id).unwrap_or(0);
    let refuse = |stream: &mut Box<dyn ShimStream>, code| {
        write_frame(
            stream.as_mut(),
            &ShimFrame::Reply {
                id,
                outcome: Outcome::error(code),
            },
        );
    };
    if !owner {
        return refuse(&mut stream, ErrorCode::PeerRefused);
    }
    let Some(Request {
        op: Op::Hello {
            secret: presented, ..
        },
        ..
    }) = request
    else {
        return refuse(&mut stream, ErrorCode::BadFrame);
    };
    let matches = secret::from_hex(&presented).is_some_and(|bytes| {
        secret::equal(&secret::secret_hash(&bytes), &config.expected_secret_hash)
    });
    if !matches {
        return refuse(&mut stream, ErrorCode::SecretMismatch);
    }
    {
        let mut guard = inner.lock().unwrap_or_else(|p| p.into_inner());
        if guard.controller.is_some() {
            drop(guard);
            return refuse(&mut stream, ErrorCode::Busy);
        }
        let Ok(writer) = stream.try_clone_stream() else {
            return;
        };
        guard.controller = Some(writer);
        guard.poll_child();
        let status = guard.status.clone();
        guard.send(&ShimFrame::Reply {
            id,
            outcome: Outcome::HelloAck {
                v: Version::CURRENT,
                status,
            },
        });
    }
    let _ = stream.set_read_bound(None);
    let inner = Arc::clone(inner);
    std::thread::spawn(move || {
        // However this thread ends (the engine left, or a panic), the
        // controller slot frees: a dead connection must never keep the
        // Shim answering `busy`.
        struct Release(Arc<Mutex<Inner>>);
        impl Drop for Release {
            fn drop(&mut self) {
                let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
                guard.controller = None;
            }
        }
        let _release = Release(Arc::clone(&inner));
        while let Some(line) = read_line(&mut reader) {
            let mut guard = inner.lock().unwrap_or_else(|p| p.into_inner());
            let frame = match serde_json::from_str::<Request>(line.trim()) {
                Ok(request) => ShimFrame::Reply {
                    id: request.id,
                    outcome: guard.dispatch(request.op),
                },
                Err(_) => ShimFrame::Reply {
                    id: 0,
                    outcome: Outcome::error(ErrorCode::BadFrame),
                },
            };
            guard.send(&frame);
        }
        drop(stream);
    });
}

/// Serve `listener` until a linger ends the Shim (see the module docs).
pub fn serve(listener: &dyn ShimListener, config: ShimConfig) -> ShimExit {
    let inner = Arc::new(Mutex::new(Inner {
        backend: crate::backends::current(),
        handle: None,
        status: ShimStatus::default(),
        controller: None,
        exit_seen: None,
        acknowledged: false,
        bypass: None,
        next_bypass: None,
    }));
    let started = Instant::now();
    loop {
        {
            let mut guard = inner.lock().unwrap_or_else(|p| p.into_inner());
            guard.poll_child();
            if guard.acknowledged {
                return ShimExit::Acknowledged;
            }
            if !guard.status.launched
                && guard.controller.is_none()
                && started.elapsed() >= config.idle_linger
            {
                return ShimExit::NeverLaunched;
            }
            if guard
                .exit_seen
                .is_some_and(|seen| seen.elapsed() >= config.exit_linger)
            {
                return ShimExit::Unacknowledged;
            }
        }
        // Story 22-4: a due bypass check enumerates OUTSIDE the lock.
        let due = inner.lock().unwrap_or_else(|p| p.into_inner()).bypass_due();
        if let Some((pid, hosts)) = due {
            let result = crate::backends::tree_remote_addrs(pid);
            inner
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .apply_bypass(result, &hosts);
        }
        match listener.accept_pending() {
            Ok(Some((stream, owner))) => admit(&inner, &config, stream, owner),
            Ok(None) | Err(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}
