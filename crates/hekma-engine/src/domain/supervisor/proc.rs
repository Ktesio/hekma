//! The supervisor's process backend (story 24-1 phase 2): the per-OS
//! direct backend, and — when a Shim program is configured and the OS has a
//! Shim transport — launches through a per-process Shim (`hekma shim`) that
//! owns the child, so the child outlives the engine and a later engine
//! reconnects to it.
//!
//! [`ProcBackend`] is a [`ProcessBackend`] whose handle is [`ProcHandle`]:
//! `Direct` (today's handle, unchanged) or `Shim` (a connection to the
//! child's Shim). The supervisor's call sites do not change. A Shim handle
//! has no teardown: dropping it (or the engine) closes the connection and
//! leaves the child running in its Shim; only an explicit stop stops it.
//!
//! Off by default: with no Shim program configured every launch is direct,
//! exactly as before. ACP launches stay direct for now (their stdout is the
//! protocol stream, which the Shim does not relay yet).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::backends;
use crate::ports::{
    BackendError, LogCapture, ProcessBackend, ProcessFingerprint, ProcessStatus, SpawnSpec,
    StdinState, StopOutcome,
};
use crate::shim::client::{ShimClient, ShimClientError};
use crate::shim::proto::ShimSpawn;

/// How long a launch waits for a fresh Shim to listen.
const SHIM_LISTEN_BOUND: Duration = Duration::from_secs(10);

/// How long one request to a Shim may take (a stop's own window is added).
const SHIM_CALL_BOUND: Duration = Duration::from_secs(10);

/// Where a Shim listens and how its secret is derived: what the spawn
/// record keeps so a later engine reconnects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShimEndpoint {
    /// The Shim's socket.
    pub socket: PathBuf,
    /// The launch id its connection secret derives from.
    pub launch_id: String,
}

/// The configured Shim program and the state directory its sockets and
/// host key live in.
#[derive(Clone, Debug)]
pub(crate) struct ShimLauncher {
    pub exe: PathBuf,
    pub state_dir: PathBuf,
}

/// The supervisor's backend.
pub(crate) struct ProcBackend {
    direct: backends::Backend,
    shim: Option<ShimLauncher>,
}

/// A supervised process: held directly, or through its Shim.
pub(crate) enum ProcHandle {
    Direct(backends::Handle),
    Shim(ShimHandle),
}

/// A connection to a child's Shim.
pub(crate) struct ShimHandle {
    client: ShimClient,
    endpoint: ShimEndpoint,
    child: ProcessFingerprint,
    has_stdin: bool,
    /// The child's exit, once seen.
    exited: Option<Option<i32>>,
}

impl std::fmt::Debug for ProcHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcHandle::Direct(_) => f.write_str("ProcHandle::Direct"),
            ProcHandle::Shim(handle) => f
                .debug_struct("ProcHandle::Shim")
                .field("child", &handle.child)
                .field("socket", &handle.endpoint.socket)
                .finish_non_exhaustive(),
        }
    }
}

fn control(op: &'static str, err: ShimClientError) -> BackendError {
    BackendError::Control {
        op,
        detail: format!("the agent's Shim: {err}"),
    }
}

/// A fresh launch id: 16 random bytes, hex.
fn launch_id() -> Result<String, BackendError> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|err| BackendError::Spawn {
        exec: "hekma shim".to_string(),
        detail: format!("no launch id could be drawn: {err}"),
    })?;
    Ok(crate::shim::secret::to_hex(&bytes))
}

impl ProcBackend {
    /// The OS's direct backend; no Shim.
    pub(crate) fn new() -> Self {
        ProcBackend {
            direct: backends::current(),
            shim: None,
        }
    }

    /// Launch supervised processes through a Shim from now on (`None`:
    /// directly). Takes effect where the OS has a Shim transport.
    pub(crate) fn set_shim(&mut self, launcher: Option<ShimLauncher>) {
        self.shim = launcher.filter(|_| backends::shim_supported());
    }

    /// The Shim endpoint a handle is held through, for the spawn record.
    pub(crate) fn shim_endpoint(&self, handle: &ProcHandle) -> Option<ShimEndpoint> {
        match handle {
            ProcHandle::Shim(shim) => Some(shim.endpoint.clone()),
            ProcHandle::Direct(_) => None,
        }
    }

    fn secret(&self, launcher: &ShimLauncher, launch_id: &str) -> Result<[u8; 32], BackendError> {
        let key = crate::shim::secret::host_key(&launcher.state_dir).map_err(|err| {
            BackendError::Spawn {
                exec: launcher.exe.display().to_string(),
                detail: format!("the Shim host key could not be read or created: {err}"),
            }
        })?;
        Ok(crate::shim::secret::connection_secret(&key, launch_id))
    }

    /// Launch `spec` through a new Shim.
    fn spawn_through_shim(
        &self,
        launcher: &ShimLauncher,
        spec: &SpawnSpec,
    ) -> Result<ShimHandle, BackendError> {
        let id = launch_id()?;
        let secret = self.secret(launcher, &id)?;
        let hash = crate::shim::secret::to_hex(&crate::shim::secret::secret_hash(&secret));
        let socket = crate::shim::socket_path(&launcher.state_dir, &id);
        let log = socket.with_extension("log");
        if let Some(dir) = socket.parent() {
            backends::shim_prepare_dir(dir).map_err(|err| BackendError::Spawn {
                exec: launcher.exe.display().to_string(),
                detail: format!("the Shim directory could not be created: {err}"),
            })?;
        }
        let args: Vec<std::ffi::OsString> = vec![
            "shim".into(),
            "--listen".into(),
            socket.clone().into_os_string(),
            "--expect-secret-hash".into(),
            hash.into(),
            "--launch-id".into(),
            id.clone().into(),
        ];
        backends::spawn_shim_process(&launcher.exe, &args, &log).map_err(|err| {
            BackendError::Spawn {
                exec: launcher.exe.display().to_string(),
                detail: format!("the Shim could not be started: {err}"),
            }
        })?;
        let mut client = connect_with_retry(&socket, &secret, SHIM_LISTEN_BOUND)
            .map_err(|err| control("shim-connect", err))?;
        // The Shim starts with a cleared environment, so a launch that
        // inherits the engine's environment carries it explicitly.
        let mut dto = ShimSpawn::of(spec);
        if spec.inherit_env {
            let mut env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
            env.extend(spec.env.clone());
            dto.env = env;
            dto.inherit_env = false;
        }
        let (pid, start_time) = client.spawn(dto).map_err(|err| BackendError::Spawn {
            exec: spec.exec.clone(),
            detail: format!("the agent's Shim could not launch it: {err}"),
        })?;
        Ok(ShimHandle {
            client,
            endpoint: ShimEndpoint {
                socket,
                launch_id: id,
            },
            child: ProcessFingerprint::new(pid, start_time),
            has_stdin: spec.pipe_stdin,
            exited: None,
        })
    }

    /// Reconnect to a recorded Shim (adoption). `Ok(None)`: no Shim answers
    /// there (gone — the caller falls back to the child's fingerprint).
    /// `Err(Refused(Busy))`: another engine holds it.
    pub(crate) fn adopt_shim(
        &self,
        endpoint: &ShimEndpoint,
        child: &ProcessFingerprint,
        state_dir: &Path,
    ) -> Result<Option<ProcHandle>, ShimClientError> {
        let key = match crate::shim::secret::host_key(state_dir) {
            Ok(key) => key,
            Err(err) => return Err(ShimClientError::Io(err)),
        };
        let secret = crate::shim::secret::connection_secret(&key, &endpoint.launch_id);
        if !endpoint.socket.exists() {
            return Ok(None);
        }
        let (client, status) =
            match crate::shim::connect(&endpoint.socket, &secret, SHIM_CALL_BOUND) {
                Ok(connected) => connected,
                Err(ShimClientError::Io(_)) => return Ok(None),
                Err(err) => return Err(err),
            };
        Ok(Some(ProcHandle::Shim(ShimHandle {
            client,
            endpoint: endpoint.clone(),
            child: *child,
            has_stdin: status.has_stdin,
            exited: status.exit.map(|exit| exit.code),
        })))
    }
}

/// What reconnecting to a recorded Shim found.
pub(crate) enum ShimReacquire {
    /// Connected: the process is held through its Shim again.
    Held(ProcHandle),
    /// Another engine holds the Shim's one connection.
    Busy,
    /// No Shim for the record (none recorded, or none answers): the caller
    /// uses the child's fingerprint, as for a direct record.
    None,
}

impl ProcBackend {
    /// Reconnect to `record`'s Shim, when it has one.
    pub(crate) fn reacquire_shim(
        &self,
        record: &crate::ports::SpawnRecord,
        state_dir: &Path,
    ) -> (ShimReacquire, Option<String>) {
        let Some(shim) = &record.shim else {
            return (ShimReacquire::None, None);
        };
        let endpoint = ShimEndpoint {
            socket: shim.socket.clone(),
            launch_id: shim.launch_id.clone(),
        };
        match self.adopt_shim(&endpoint, &record.fingerprint, state_dir) {
            Ok(Some(handle)) => (ShimReacquire::Held(handle), None),
            Ok(None) => (ShimReacquire::None, None),
            Err(ShimClientError::Refused(crate::shim::proto::ErrorCode::Busy)) => {
                (ShimReacquire::Busy, None)
            }
            Err(err) => (
                ShimReacquire::None,
                Some(format!(
                    "its Shim could not be reconnected ({err}); the process is re-held by its \
                     fingerprint"
                )),
            ),
        }
    }
}

fn connect_with_retry(
    socket: &Path,
    secret: &[u8; 32],
    bound: Duration,
) -> Result<ShimClient, ShimClientError> {
    let deadline = std::time::Instant::now() + bound;
    loop {
        match crate::shim::connect(socket, secret, SHIM_CALL_BOUND) {
            Ok((client, _)) => return Ok(client),
            Err(ShimClientError::Io(err)) if std::time::Instant::now() < deadline => {
                let _ = err;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(err),
        }
    }
}

impl ShimHandle {
    fn poll(&mut self) -> Result<ProcessStatus, BackendError> {
        if let Some(code) = self.exited {
            return Ok(ProcessStatus::Exited { code });
        }
        if let Some(exit) = self.client.take_pushed_exit() {
            self.exited = Some(exit.code);
            let _ = self.client.ack_exit();
            return Ok(ProcessStatus::Exited { code: exit.code });
        }
        match self.client.poll().map_err(|err| control("poll", err))? {
            Some(exit) => {
                self.exited = Some(exit.code);
                // The exit is taken: the Shim may leave.
                let _ = self.client.ack_exit();
                Ok(ProcessStatus::Exited { code: exit.code })
            }
            None => Ok(ProcessStatus::Alive),
        }
    }
}

impl ProcessBackend for ProcBackend {
    type Handle = ProcHandle;

    fn spawn(&self, spec: &SpawnSpec) -> Result<ProcHandle, BackendError> {
        match &self.shim {
            // ACP's stdout is the protocol stream, which the Shim does not
            // relay yet: those launches stay direct.
            Some(launcher) if !spec.pipe_stdout => {
                Ok(ProcHandle::Shim(self.spawn_through_shim(launcher, spec)?))
            }
            _ => Ok(ProcHandle::Direct(self.direct.spawn(spec)?)),
        }
    }

    fn stop(
        &self,
        handle: &mut ProcHandle,
        graceful_window: Duration,
    ) -> Result<StopOutcome, BackendError> {
        match handle {
            ProcHandle::Direct(handle) => self.direct.stop(handle, graceful_window),
            ProcHandle::Shim(shim) => {
                if shim.exited.is_some() {
                    return Ok(StopOutcome { forced: false });
                }
                let forced = shim
                    .client
                    .stop(graceful_window)
                    .map_err(|err| control("stop", err))?;
                let _ = shim.poll();
                Ok(StopOutcome { forced })
            }
        }
    }

    fn poll(&self, handle: &mut ProcHandle) -> Result<ProcessStatus, BackendError> {
        match handle {
            ProcHandle::Direct(handle) => self.direct.poll(handle),
            ProcHandle::Shim(shim) => shim.poll(),
        }
    }

    fn pause(&self, handle: &mut ProcHandle) -> Result<(), BackendError> {
        match handle {
            ProcHandle::Direct(handle) => self.direct.pause(handle),
            ProcHandle::Shim(shim) => shim.client.pause().map_err(|err| control("pause", err)),
        }
    }

    fn resume(&self, handle: &mut ProcHandle) -> Result<(), BackendError> {
        match handle {
            ProcHandle::Direct(handle) => self.direct.resume(handle),
            ProcHandle::Shim(shim) => shim.client.resume().map_err(|err| control("resume", err)),
        }
    }

    fn pid(&self, handle: &ProcHandle) -> u32 {
        match handle {
            ProcHandle::Direct(handle) => self.direct.pid(handle),
            ProcHandle::Shim(shim) => shim.child.pid,
        }
    }

    fn fingerprint(&self, handle: &ProcHandle) -> ProcessFingerprint {
        match handle {
            ProcHandle::Direct(handle) => self.direct.fingerprint(handle),
            ProcHandle::Shim(shim) => shim.child,
        }
    }

    fn adopt(
        &self,
        fingerprint: &ProcessFingerprint,
        detached: bool,
    ) -> Result<Option<ProcHandle>, BackendError> {
        Ok(self
            .direct
            .adopt(fingerprint, detached)?
            .map(ProcHandle::Direct))
    }

    fn self_fingerprint(&self) -> Option<ProcessFingerprint> {
        self.direct.self_fingerprint()
    }

    fn probe_alive(&self, fingerprint: &ProcessFingerprint) -> Option<bool> {
        self.direct.probe_alive(fingerprint)
    }

    fn has_stdin(&self, handle: &ProcHandle) -> bool {
        match handle {
            ProcHandle::Direct(handle) => self.direct.has_stdin(handle),
            ProcHandle::Shim(shim) => shim.has_stdin && shim.exited.is_none(),
        }
    }

    fn stdin_timed_out(&self, handle: &ProcHandle) -> bool {
        match handle {
            ProcHandle::Direct(handle) => self.direct.stdin_timed_out(handle),
            ProcHandle::Shim(_) => false,
        }
    }

    fn write_stdin(&self, handle: &mut ProcHandle, data: &[u8]) -> Result<(), BackendError> {
        match handle {
            ProcHandle::Direct(handle) => self.direct.write_stdin(handle, data),
            ProcHandle::Shim(shim) => shim
                .client
                .write_stdin(data)
                .map_err(|err| control("write-stdin", err)),
        }
    }

    fn log_capture(&self, handle: &ProcHandle) -> Option<LogCapture> {
        match handle {
            ProcHandle::Direct(handle) => self.direct.log_capture(handle),
            // The capture runs inside the Shim; its files are on disk.
            ProcHandle::Shim(_) => None,
        }
    }

    fn take_stdin(&self, handle: &mut ProcHandle) -> Option<StdinState> {
        match handle {
            ProcHandle::Direct(handle) => self.direct.take_stdin(handle),
            ProcHandle::Shim(_) => None,
        }
    }

    fn take_stdout(&self, handle: &mut ProcHandle) -> Option<std::process::ChildStdout> {
        match handle {
            ProcHandle::Direct(handle) => self.direct.take_stdout(handle),
            ProcHandle::Shim(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::ports::LogCaptureMode;

    fn spec(pipe_stdout: bool) -> SpawnSpec {
        SpawnSpec {
            exec: hekma_conformance::fake_agent_bin()
                .to_string_lossy()
                .into_owned(),
            args: vec!["--linger-ms".to_string(), "600000".to_string()],
            env: BTreeMap::new(),
            working_dir: std::env::temp_dir(),
            log_file: None,
            attributed_log_path: None,
            stderr_log_file: None,
            instance_name: "proc-test".to_string(),
            pipe_stdin: true,
            pipe_stdout,
            detach: false,
            inherit_env: true,
            capture_mode: LogCaptureMode::default(),
        }
    }

    fn with_shim(exe: PathBuf, state: &Path) -> ProcBackend {
        let mut backend = ProcBackend::new();
        backend.set_shim(Some(ShimLauncher {
            exe,
            state_dir: state.to_path_buf(),
        }));
        backend
    }

    #[test]
    fn direct_handles_debug_by_kind_and_report_no_shim() {
        let backend = ProcBackend::new();
        let mut handle = backend.spawn(&spec(false)).unwrap();
        assert_eq!(format!("{handle:?}"), "ProcHandle::Direct");
        assert!(backend.shim_endpoint(&handle).is_none());
        assert!(backend.has_stdin(&handle));
        assert!(!backend.stdin_timed_out(&handle));
        let _ = backend.stop(&mut handle, Duration::from_millis(500));
    }

    #[test]
    fn a_missing_shim_program_fails_the_launch_and_acp_stays_direct() {
        if !backends::shim_supported() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let backend = with_shim(state.path().join("no-such-shim"), state.path());
        let err = backend.spawn(&spec(false)).unwrap_err();
        assert!(
            err.to_string().contains("the Shim could not be started"),
            "{err}"
        );
        // An ACP launch (piped stdout) never goes through the Shim.
        let mut handle = backend.spawn(&spec(true)).unwrap();
        assert!(matches!(handle, ProcHandle::Direct(_)));
        let _ = backend.stop(&mut handle, Duration::from_millis(500));
    }

    #[test]
    fn a_shim_launch_debugs_its_socket_and_answers_every_verb() {
        if !backends::shim_supported() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let backend = with_shim(
            hekma_conformance::test_support::shim_host_bin(),
            state.path(),
        );
        let mut handle = backend.spawn(&spec(false)).unwrap();
        let shown = format!("{handle:?}");
        assert!(
            shown.starts_with("ProcHandle::Shim") && shown.contains("socket"),
            "{shown}"
        );
        let endpoint = backend.shim_endpoint(&handle).unwrap();
        assert_eq!(endpoint.launch_id.len(), 32);
        assert!(backend.pid(&handle) > 0);
        assert_eq!(backend.fingerprint(&handle).pid, backend.pid(&handle));
        assert!(backend.has_stdin(&handle));
        assert!(!backend.stdin_timed_out(&handle));
        assert!(backend.log_capture(&handle).is_none());
        assert!(backend.take_stdin(&mut handle).is_none());
        assert!(backend.take_stdout(&mut handle).is_none());
        backend.write_stdin(&mut handle, b"hi\n").unwrap();
        backend.pause(&mut handle).unwrap();
        backend.resume(&mut handle).unwrap();
        assert_eq!(backend.poll(&mut handle).unwrap(), ProcessStatus::Alive);
        backend
            .stop(&mut handle, Duration::from_millis(500))
            .unwrap();
        assert!(backend.poll(&mut handle).unwrap().is_exited());
        // Stopped already: a second stop is a no-op, stdin is gone.
        assert!(!backend.stop(&mut handle, Duration::ZERO).unwrap().forced);
        assert!(!backend.has_stdin(&handle));
        assert!(backend.self_fingerprint().is_some());
        assert_eq!(
            backend.probe_alive(&backend.fingerprint(&handle)),
            Some(false)
        );
    }

    #[test]
    fn reconnecting_finds_no_shim_a_foreign_secret_or_no_record() {
        if !backends::shim_supported() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let backend = ProcBackend::new();
        let child = ProcessFingerprint::new(1, 1);
        let mut record = crate::ports::SpawnRecord {
            name: crate::domain::InstanceName::new("ghost").unwrap(),
            fingerprint: child,
            restart_policy: crate::domain::RestartPolicy::Never,
            restart_count: 0,
            last_known_cause: None,
            detach: false,
            acp_session_id: None,
            run_id: None,
            owner: None,
            lease: None,
            shim: None,
        };
        assert!(matches!(
            backend.reacquire_shim(&record, state.path()).0,
            ShimReacquire::None
        ));
        // A recorded socket nobody listens on.
        record.shim = Some(crate::ports::ShimRecord {
            socket: state.path().join("shims").join("s-gone.sock"),
            launch_id: "aa".repeat(16),
        });
        let (found, note) = backend.reacquire_shim(&record, state.path());
        assert!(matches!(found, ShimReacquire::None) && note.is_none());
        // A Shim whose secret is not this launch's: refused, with a note.
        let socket = state.path().join("shims").join("s-other.sock");
        let listener = backends::shim_listen(&socket).unwrap();
        let config = crate::shim::server::ShimConfig {
            expected_secret_hash: [9u8; 32],
            idle_linger: Duration::from_secs(2),
            exit_linger: Duration::from_secs(2),
        };
        let server =
            std::thread::spawn(move || crate::shim::server::serve(listener.as_ref(), config));
        record.shim = Some(crate::ports::ShimRecord {
            socket,
            launch_id: "bb".repeat(16),
        });
        let (found, note) = backend.reacquire_shim(&record, state.path());
        assert!(matches!(found, ShimReacquire::None));
        assert!(
            note.is_some_and(|note| note.contains("could not be reconnected")),
            "a refused reconnect is said"
        );
        let _ = server.join();
    }

    #[test]
    fn every_launch_and_reconnect_failure_is_a_typed_error() {
        if !backends::shim_supported() {
            return;
        }
        // A state directory that is a file: no host key can be made.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let backend = with_shim(hekma_conformance::test_support::shim_host_bin(), &file);
        let err = backend.spawn(&spec(false)).unwrap_err().to_string();
        assert!(err.contains("host key"), "{err}");
        let endpoint = ShimEndpoint {
            socket: file.join("s.sock"),
            launch_id: "cc".repeat(16),
        };
        assert!(backend
            .adopt_shim(&endpoint, &ProcessFingerprint::new(1, 1), &file)
            .is_err());
        // A `shims` path that is a file: the socket directory cannot be made.
        let state = tempfile::tempdir().unwrap();
        std::fs::write(state.path().join("shims"), b"x").unwrap();
        let backend = with_shim(
            hekma_conformance::test_support::shim_host_bin(),
            state.path(),
        );
        let err = backend.spawn(&spec(false)).unwrap_err().to_string();
        assert!(err.contains("Shim directory"), "{err}");
        // A Shim that cannot launch the agent: the refusal names it.
        let state = tempfile::tempdir().unwrap();
        let backend = with_shim(
            hekma_conformance::test_support::shim_host_bin(),
            state.path(),
        );
        let mut missing = spec(false);
        missing.exec = state
            .path()
            .join("no-such-agent")
            .to_string_lossy()
            .into_owned();
        let err = backend.spawn(&missing).unwrap_err().to_string();
        assert!(err.contains("could not launch it"), "{err}");
        // A Shim that dies: its requests fail as control errors.
        let mut handle = backend.spawn(&spec(false)).unwrap();
        let socket = backend.shim_endpoint(&handle).unwrap().socket;
        let shim = std::process::Command::new("pgrep")
            .args(["-f", &socket.to_string_lossy()])
            .output()
            .unwrap();
        let shim_pid = String::from_utf8_lossy(&shim.stdout)
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_string();
        std::process::Command::new("kill")
            .args(["-9", &shim_pid])
            .status()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let err = backend.poll(&mut handle).unwrap_err().to_string();
        assert!(err.contains("the agent's Shim"), "{err}");
        // The child outlived its Shim; end it directly.
        let child = backend.fingerprint(&handle);
        if let Ok(Some(mut direct)) = backend.adopt(&child, false) {
            let _ = backend.stop(&mut direct, Duration::from_millis(500));
        }
        // Direct handles hand their pipes over.
        let plain = ProcBackend::new();
        let mut direct = plain.spawn(&spec(false)).unwrap();
        assert!(plain.take_stdin(&mut direct).is_some());
        assert!(plain.take_stdout(&mut direct).is_none());
        let _ = plain.stop(&mut direct, Duration::from_millis(500));
    }
}
