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
            bypass: None,
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
        let replaced = listen(&path).err().map(|err| err.kind());
        assert_eq!(
            replaced,
            Some(io::ErrorKind::AddrInUse),
            "a live socket stays"
        );
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
            let found = engine.take_pushed_exit().or_else(|| engine.poll().unwrap());
            if let Some(exit) = found {
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

    /// Story 22-4: a fake agent holding one TCP connection to `addr`.
    fn holding(addr: std::net::SocketAddr, log: &Path) -> ShimSpawn {
        let mut dto = spec("true", false, log);
        dto.exec = hekma_conformance::fake_agent_bin().display().to_string();
        dto.args = vec!["--hold-tcp".into(), addr.to_string()];
        dto
    }

    fn listed(addr: std::net::SocketAddr) -> crate::domain::bypass::ProviderHost {
        crate::domain::bypass::ProviderHost {
            host: format!("gw-{}", addr.port()),
            port: addr.port(),
            addrs: vec![addr.ip()],
        }
    }

    /// Wait for the Shim's report to satisfy `done`, reading pushes and
    /// `status`.
    fn report_until(
        engine: &mut ShimClient,
        done: impl Fn(&crate::shim::proto::BypassReport) -> bool,
    ) -> crate::shim::proto::BypassReport {
        let deadline = std::time::Instant::now() + BOUND;
        loop {
            let report = engine.status().unwrap().bypass;
            if let Some(pushed) = engine.take_pushed_bypass() {
                if done(&pushed) {
                    return pushed;
                }
            }
            if done(&report) {
                return report;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "last report {report:?}"
            );
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    #[test]
    fn the_shim_reports_a_listed_host_its_tree_reached_and_never_one_outside_it() {
        let gateway = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let outside = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (gw, out) = (gateway.local_addr().unwrap(), outside.local_addr().unwrap());
        // This test process (outside the Shim's tree) holds a connection
        // to the second listed endpoint the whole time.
        let _sibling = std::net::TcpStream::connect(out).unwrap();
        let shim = start(Duration::from_secs(30), Duration::from_secs(30));
        let log = shim.path.parent().unwrap().join("agent.log");
        let (mut engine, _) =
            ShimClient::hello(connect(&shim.path).unwrap(), &shim.secret, BOUND).unwrap();
        let mut dto = holding(gw, &log);
        dto.bypass = Some(Box::new(crate::shim::proto::BypassConfig {
            interval_ms: 50,
            hosts: vec![listed(gw), listed(out)],
        }));
        engine.spawn(dto).unwrap();
        let report = report_until(&mut engine, |r| !r.hosts.is_empty());
        assert_eq!(report.state, "active");
        assert_eq!(report.hosts, vec![format!("gw-{}", gw.port())]);
        // Polls keep running; the sibling never joins, and the host is
        // reported once (set-once).
        std::thread::sleep(Duration::from_millis(300));
        let status = engine.status().unwrap();
        assert_eq!(status.bypass.hosts, report.hosts, "{status:?}");
        let _ = engine.stop(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn a_shim_launched_with_no_list_reports_nothing_until_one_is_pushed() {
        let gateway = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let gw = gateway.local_addr().unwrap();
        let shim = start(Duration::from_secs(30), Duration::from_secs(30));
        let log = shim.path.parent().unwrap().join("agent.log");
        let (mut engine, _) =
            ShimClient::hello(connect(&shim.path).unwrap(), &shim.secret, BOUND).unwrap();
        engine.spawn(holding(gw, &log)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let status = engine.status().unwrap();
        assert_eq!(
            status.bypass.state, "unreported",
            "no list: never a clean read"
        );
        engine
            .set_bypass_hosts(crate::shim::proto::BypassConfig {
                interval_ms: 50,
                hosts: vec![listed(gw)],
            })
            .unwrap();
        let report = report_until(&mut engine, |r| !r.hosts.is_empty());
        assert_eq!(report.state, "active");
        // A reconnecting engine reads the report from `status`.
        drop(engine);
        std::thread::sleep(Duration::from_millis(100));
        let (mut engine, status) =
            ShimClient::hello(connect(&shim.path).unwrap(), &shim.secret, BOUND).unwrap();
        assert_eq!(status.bypass.hosts, report.hosts);
        let _ = engine.stop(Duration::from_secs(2)).unwrap();
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

    /// One raw line out, one frame back (the wire as an engine sees it).
    struct Raw {
        writer: UnixStream,
        reader: std::io::BufReader<UnixStream>,
    }

    impl Raw {
        fn open(path: &Path) -> Raw {
            let writer = UnixStream::connect(path).unwrap();
            writer.set_read_timeout(Some(BOUND)).unwrap();
            let reader = std::io::BufReader::new(writer.try_clone().unwrap());
            Raw { writer, reader }
        }

        fn ask(&mut self, line: &str) -> serde_json::Value {
            use std::io::{BufRead, Write};
            self.writer.write_all(line.as_bytes()).unwrap();
            self.writer.write_all(b"\n").unwrap();
            loop {
                let mut text = String::new();
                self.reader.read_line(&mut text).unwrap();
                let frame: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
                if frame["frame"] == "reply" {
                    return frame;
                }
            }
        }

        fn code(&mut self, line: &str) -> String {
            let frame = self.ask(line);
            frame["outcome"]["code"].as_str().unwrap_or("").to_string()
        }
    }

    #[test]
    fn every_request_out_of_turn_is_refused_by_its_code() {
        let shim = start(Duration::from_secs(30), Duration::from_secs(30));
        // A first frame that is not a hello, and one that is not JSON.
        assert_eq!(
            Raw::open(&shim.path).code(r#"{"id":3,"op":"status"}"#),
            "bad-frame"
        );
        assert_eq!(Raw::open(&shim.path).code("not json"), "bad-frame");
        let mut raw = Raw::open(&shim.path);
        let hello = format!(
            r#"{{"id":1,"op":"hello","v":{{"major":1,"minor":0}},"secret":"{}","engine_pid":1}}"#,
            secret::to_hex(&shim.secret)
        );
        assert_eq!(raw.ask(&hello)["outcome"]["kind"], "hello_ack");
        // Before a launch: everything but status is out of turn.
        assert_eq!(
            raw.code(&hello.replace(r#""id":1"#, r#""id":2"#)),
            "wrong-state"
        );
        for op in [
            r#"{"id":4,"op":"stop","graceful_ms":10}"#,
            r#"{"id":5,"op":"pause"}"#,
            r#"{"id":6,"op":"resume"}"#,
            r#"{"id":7,"op":"poll"}"#,
            r#"{"id":8,"op":"write_stdin","data":"6869"}"#,
            r#"{"id":9,"op":"ack_exit"}"#,
        ] {
            assert_eq!(raw.code(op), "wrong-state", "{op}");
        }
        assert_eq!(
            raw.code(r#"{"id":10,"op":"write_stdin","data":"zz"}"#),
            "bad-frame"
        );
        assert_eq!(raw.code("{broken"), "bad-frame");
        let status = raw.ask(r#"{"id":11,"op":"status"}"#);
        assert_eq!(status["outcome"]["status"]["launched"], false);
        // A spawn the backend cannot take (an unknown capture mode).
        let dir = tempfile::tempdir().unwrap();
        let mut bad = spec("exit 0", false, &dir.path().join("agent.log"));
        bad.capture_mode = "sideways".into();
        let line = |id: u64, spec: &ShimSpawn| {
            format!(
                r#"{{"id":{id},"op":"spawn","spec":{}}}"#,
                serde_json::to_string(spec).unwrap()
            )
        };
        assert_eq!(raw.code(&line(12, &bad)), "bad-frame");
        // A launch, then a second one; then the child exits.
        let good = spec("sleep 0.2", true, &dir.path().join("agent.log"));
        assert_eq!(raw.ask(&line(13, &good))["outcome"]["kind"], "spawned");
        assert_eq!(raw.code(&line(14, &good)), "wrong-state");
        assert_eq!(
            raw.ask(r#"{"id":15,"op":"pause"}"#)["outcome"]["kind"],
            "done"
        );
        assert_eq!(
            raw.ask(r#"{"id":16,"op":"resume"}"#)["outcome"]["kind"],
            "done"
        );
        let deadline = std::time::Instant::now() + BOUND;
        while raw.ask(r#"{"id":17,"op":"poll"}"#)["outcome"]["exit"].is_null() {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never exited"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            raw.code(r#"{"id":18,"op":"write_stdin","data":"6869"}"#),
            "child-gone"
        );
        assert_eq!(
            raw.ask(r#"{"id":19,"op":"ack_exit"}"#)["outcome"]["kind"],
            "done"
        );
        assert_eq!(shim.thread.join().unwrap(), ShimExit::Acknowledged);
    }

    /// A fake Shim on one end of a socket pair: answers each request with
    /// the next scripted frame line.
    fn fake_shim(frames: Vec<String>) -> Box<dyn ShimStream> {
        let (engine, shim) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let mut reader = std::io::BufReader::new(shim.try_clone().unwrap());
            let mut writer = shim;
            for frame in frames {
                let mut request = String::new();
                if reader.read_line(&mut request).unwrap_or(0) == 0 {
                    return;
                }
                let _ = writer.write_all(frame.as_bytes());
            }
        });
        Box::new(engine)
    }

    #[test]
    fn the_client_reports_a_foreign_version_an_unexpected_answer_and_a_closed_shim() {
        let ack = |major: u32| {
            format!(
                "{}\n",
                serde_json::json!({"frame":"reply","id":1,"outcome":{"kind":"hello_ack",
                    "v":{"major":major,"minor":0},"status":crate::shim::proto::ShimStatus::default()}})
            )
        };
        let foreign = ShimClient::hello(fake_shim(vec![ack(9)]), &[1; 32], BOUND).unwrap_err();
        assert!(matches!(foreign, ShimClientError::Version(_)), "{foreign}");
        assert!(foreign.to_string().contains("9.0"));
        let odd = r#"{"frame":"reply","id":1,"outcome":{"kind":"done"}}"#.to_string() + "\n";
        let unexpected = ShimClient::hello(fake_shim(vec![odd.clone()]), &[1; 32], BOUND);
        assert!(matches!(unexpected, Err(ShimClientError::Unexpected)));
        // A reply to another id is skipped; a pushed exit is kept; every
        // verb answered with the wrong outcome is Unexpected; then EOF.
        let stale = r#"{"frame":"reply","id":99,"outcome":{"kind":"done"}}"#.to_string() + "\n";
        let pushed =
            r#"{"frame":"exit","exit":{"code":3,"at":"2026-11-14T00:00:00Z"}}"#.to_string() + "\n";
        let reply = |id: u64, outcome: &str| {
            format!(
                "{}\n",
                serde_json::json!({"frame":"reply","id":id,"outcome":{"kind":outcome}})
            )
        };
        let mut frames = vec![stale + &pushed + &ack(1)];
        for (id, outcome) in [(2, "done"), (3, "done"), (4, "done"), (5, "spawned_never")] {
            frames.push(reply(id, outcome));
        }
        frames.push(reply(6, "done"));
        frames.push(reply(7, "stopped_never"));
        let (mut client, _) = ShimClient::hello(fake_shim(frames), &[1; 32], BOUND).unwrap();
        assert!(format!("{client:?}").contains("ShimClient"));
        assert_eq!(
            client.take_pushed_exit().and_then(|exit| exit.code),
            Some(3)
        );
        assert!(matches!(client.poll(), Err(ShimClientError::Unexpected)));
        assert!(matches!(client.status(), Err(ShimClientError::Unexpected)));
        assert!(matches!(
            client.stop(Duration::from_millis(1)),
            Err(ShimClientError::Unexpected)
        ));
        assert!(client.pause().is_err());
        assert!(matches!(
            client.spawn(spec("exit 0", false, Path::new("/tmp/x.log"))),
            Err(ShimClientError::Unexpected)
        ));
        assert!(client.resume().is_err());
        assert!(matches!(client.ack_exit(), Err(ShimClientError::Io(_))));
    }
}
