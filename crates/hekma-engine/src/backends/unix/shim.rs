//! The Shim's Unix transport (story 24-1): a Unix domain socket in the
//! state directory's `shims/` folder (mode `0700`, checked like the
//! secrets file), a short hashed name under `sun_path`'s limit, a stale
//! socket probed and removed before binding, and the peer's uid checked
//! against the Shim's own on every connection (`getpeereid` on macOS and
//! the BSDs, `SO_PEERCRED` on Linux) before any protocol byte.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::shim::server::{ShimListener, ShimStream};

/// Create (or check) the `0700` socket directory.
pub fn prepare_dir(dir: &Path) -> io::Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let mode = std::fs::metadata(dir)?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "the Shim socket directory {} must be readable only by its owner (mode {:o}); \
                 fix it with: chmod 700 '{}'",
                dir.display(),
                mode & 0o777,
                dir.display()
            ),
        ));
    }
    Ok(())
}

/// The uid of a connected socket's peer.
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let creds =
            nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)
                .map_err(io::Error::from)?;
        Ok(creds.uid())
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let (uid, _) = nix::unistd::getpeereid(stream).map_err(io::Error::from)?;
        Ok(uid.as_raw())
    }
}

impl ShimStream for UnixStream {
    fn try_clone_stream(&self) -> io::Result<Box<dyn ShimStream>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn set_read_bound(&self, bound: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(bound)
    }
}

/// A listening Shim socket.
pub struct UnixShimListener {
    listener: UnixListener,
    owner: u32,
    path: PathBuf,
}

impl ShimListener for UnixShimListener {
    fn accept_pending(&self) -> io::Result<Option<(Box<dyn ShimStream>, bool)>> {
        match self.listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                let owner = peer_uid(&stream).is_ok_and(|uid| uid == self.owner);
                Ok(Some((Box::new(stream), owner)))
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(err) => Err(err),
        }
    }
}

impl Drop for UnixShimListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Listen on `path` (in a `0700` directory). A socket file that answers
/// belongs to a live Shim and is refused; one that does not is stale and
/// removed first.
pub fn listen(path: &Path) -> io::Result<UnixShimListener> {
    if let Some(dir) = path.parent() {
        prepare_dir(dir)?;
    }
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("a live Shim already listens on {}", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    Ok(UnixShimListener {
        listener,
        owner: nix::unistd::getuid().as_raw(),
        path: path.to_path_buf(),
    })
}

/// Connect to the Shim on `path`.
pub fn connect(path: &Path) -> io::Result<Box<dyn ShimStream>> {
    Ok(Box::new(UnixStream::connect(path)?))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::shim::client::{ShimClient, ShimClientError};
    use crate::shim::proto::{ErrorCode, ShimSpawn};
    use crate::shim::secret;
    use crate::shim::server::{serve, ShimConfig, ShimExit};

    const BOUND: Duration = Duration::from_secs(10);

    struct Running {
        _dir: tempfile::TempDir,
        path: PathBuf,
        secret: [u8; 32],
        thread: std::thread::JoinHandle<ShimExit>,
    }

    fn start(idle: Duration, exit: Duration) -> Running {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shims").join("s-test.sock");
        let key = secret::host_key(dir.path()).unwrap();
        let secret = secret::connection_secret(&key, "launch-1");
        let listener = listen(&path).unwrap();
        let config = ShimConfig {
            expected_secret_hash: secret::secret_hash(&secret),
            idle_linger: idle,
            exit_linger: exit,
        };
        let thread = std::thread::spawn(move || serve(&listener, config));
        Running {
            _dir: dir,
            path,
            secret,
            thread,
        }
    }

    fn spec(script: &str, pipe_stdin: bool, log: &Path) -> ShimSpawn {
        let mut env = std::collections::BTreeMap::new();
        env.insert("PATH".into(), "/usr/bin:/bin".into());
        env.insert("HEKMA_RUN_KEY".into(), "SENTINEL-RUN-KEY".into());
        ShimSpawn {
            exec: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env,
            working_dir: std::env::temp_dir(),
            log_file: Some(log.to_path_buf()),
            attributed_log_path: Some(log.with_extension("attributed")),
            stderr_log_file: Some(log.with_extension("stderr")),
            instance_name: "shimmed".into(),
            pipe_stdin,
            detach: false,
            pipe_stdout: false,
            inherit_env: false,
            capture_mode: "full".into(),
        }
    }

    #[test]
    fn the_socket_directory_is_owner_only_and_a_stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let shims = dir.path().join("shims");
        prepare_dir(&shims).unwrap();
        assert_eq!(
            std::fs::metadata(&shims).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::set_permissions(&shims, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(prepare_dir(&shims)
            .unwrap_err()
            .to_string()
            .contains("chmod 700"));
        std::fs::set_permissions(&shims, std::fs::Permissions::from_mode(0o700)).unwrap();
        // A stale file (nothing listening) is replaced; a live socket is not.
        let path = shims.join("s-stale.sock");
        std::fs::write(&path, b"").unwrap();
        let live = listen(&path).unwrap();
        match listen(&path) {
            Err(err) => assert_eq!(err.kind(), io::ErrorKind::AddrInUse),
            Ok(_) => panic!("a live socket must not be replaced"),
        }
        drop(live);
        assert!(!path.exists(), "the listener removes its socket");
    }

    #[test]
    fn a_wrong_secret_is_refused_and_a_second_engine_is_busy() {
        let shim = start(Duration::from_secs(30), Duration::from_secs(30));
        let wrong = [7u8; 32];
        let refused = ShimClient::hello(connect(&shim.path).unwrap(), &wrong, BOUND).unwrap_err();
        assert!(matches!(
            refused,
            ShimClientError::Refused(ErrorCode::SecretMismatch)
        ));
        let (mut first, status) =
            ShimClient::hello(connect(&shim.path).unwrap(), &shim.secret, BOUND).unwrap();
        assert!(!status.launched);
        let busy =
            ShimClient::hello(connect(&shim.path).unwrap(), &shim.secret, BOUND).unwrap_err();
        assert!(matches!(busy, ShimClientError::Refused(ErrorCode::Busy)));
        // The first controller leaves; a second may connect.
        assert!(first.status().is_ok());
        drop(first);
        std::thread::sleep(Duration::from_millis(200));
        let (_second, _) =
            ShimClient::hello(connect(&shim.path).unwrap(), &shim.secret, BOUND).unwrap();
    }

    #[test]
    fn the_child_outlives_its_engine_and_a_reconnect_gets_the_buffered_exit_and_stdin() {
        let shim = start(Duration::from_secs(30), Duration::from_secs(30));
        let log = shim.path.parent().unwrap().join("agent.log");
        let (mut engine, _) =
            ShimClient::hello(connect(&shim.path).unwrap(), &shim.secret, BOUND).unwrap();
        let (pid, _start) = engine
            .spawn(spec("read line; echo \"got $line\"; exit 3", true, &log))
            .unwrap();
        assert!(pid > 0);
        let status = engine.status().unwrap();
        assert!(status.launched && status.env_dropped && status.has_stdin);
        assert!(!format!("{status:?}").contains("SENTINEL"));
        // The engine goes away; the child keeps running in the Shim.
        drop(engine);
        std::thread::sleep(Duration::from_millis(200));
        // A new engine reconnects: the child is alive and its stdin works.
        let (mut engine, status) =
            ShimClient::hello(connect(&shim.path).unwrap(), &shim.secret, BOUND).unwrap();
        assert!(status.launched && status.exit.is_none(), "{status:?}");
        engine.write_stdin(b"hello\n").unwrap();
        let deadline = std::time::Instant::now() + BOUND;
        let exit = loop {
            if let Some(exit) = engine.take_pushed_exit() {
                break exit;
            }
            if let Some(exit) = engine.poll().unwrap() {
                break exit;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the child never exited"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(exit.code, Some(3));
        assert!(std::fs::read_to_string(&log).unwrap().contains("got hello"));
        // The exit is kept for an engine that was not connected, too.
        drop(engine);
        std::thread::sleep(Duration::from_millis(100));
        let (mut engine, status) =
            ShimClient::hello(connect(&shim.path).unwrap(), &shim.secret, BOUND).unwrap();
        assert_eq!(status.exit.as_ref().and_then(|e| e.code), Some(3));
        assert!(matches!(
            engine.write_stdin(b"x\n").unwrap_err(),
            ShimClientError::Refused(ErrorCode::ChildGone)
        ));
        engine.ack_exit().unwrap();
        assert_eq!(shim.thread.join().unwrap(), ShimExit::Acknowledged);
    }

    #[test]
    fn stop_runs_the_backends_ladder_and_the_lingers_end_an_idle_or_unacknowledged_shim() {
        let shim = start(Duration::from_secs(30), Duration::from_millis(300));
        let log = shim.path.parent().unwrap().join("agent.log");
        let (mut engine, _) =
            ShimClient::hello(connect(&shim.path).unwrap(), &shim.secret, BOUND).unwrap();
        engine.spawn(spec("sleep 30", false, &log)).unwrap();
        assert!(matches!(
            engine.spawn(spec("true", false, &log)).unwrap_err(),
            ShimClientError::Refused(ErrorCode::WrongState)
        ));
        let mut half = spec("true", false, &log);
        half.stderr_log_file = None;
        assert!(
            half.spec().is_none(),
            "half-set capture paths are refused, never a panic"
        );
        engine.pause().unwrap();
        engine.resume().unwrap();
        let _forced = engine.stop(Duration::from_secs(2)).unwrap();
        assert!(engine.poll().unwrap().is_some(), "stopped");
        drop(engine);
        assert_eq!(shim.thread.join().unwrap(), ShimExit::Unacknowledged);
        // Never launched: the idle linger ends it.
        let idle = start(Duration::from_millis(200), Duration::from_secs(30));
        assert_eq!(idle.thread.join().unwrap(), ShimExit::NeverLaunched);
    }
}
