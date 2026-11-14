//! Supervision through a Shim (story 24-1 phase 2): with
//! `EngineOptions::shim_exe` set, a supervised launch goes through a
//! per-process Shim that owns the child. The spawn record names the Shim;
//! dropping the engine leaves the child running; the next engine reconnects
//! to the Shim (adoption by reconnect) and its stop ends the child.
//!
//! Unix only for now (the Shim transport is a Unix socket): elsewhere the
//! test returns early, data-driven — the Windows pipe is a later phase.

use std::path::Path;
use std::time::{Duration, Instant};

use hekma_engine::{AdapterRef, Engine, EngineOptions, LifecycleState};
use tempfile::TempDir;

fn open(state: &Path) -> Engine {
    Engine::open_with_options(
        Some(state.to_path_buf()),
        EngineOptions {
            shim_exe: Some(hekma_conformance::test_support::shim_host_bin()),
            ..EngineOptions::default()
        },
    )
    .expect("open an engine with a Shim program")
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[test]
fn a_shim_launched_agent_outlives_its_engine_and_the_next_engine_reconnects_and_stops_it() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    hekma_conformance::test_support::ManifestFixture::new("svc", &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("interaction")
        .write(manifest.path());
    let pid = {
        let engine = open(state.path());
        let facade = engine.blocking();
        facade
            .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
            .unwrap();
        let started = facade.start("svc").unwrap();
        assert_eq!(started.state, LifecycleState::Running);
        // The record names the Shim it was launched through.
        let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
        let (pid, socket, launch): (i64, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT pid, shim_socket, shim_launch_id FROM agent_runtime",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let pid = pid as u32;
        let socket = socket.expect("the spawn record names the Shim socket");
        assert!(launch.is_some_and(|id| id.len() == 32));
        assert!(Path::new(&socket).exists(), "the Shim listens");
        pid
    };
    // The engine is gone; the child is not.
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(pid), "the agent outlives its engine through its Shim");
    // The next engine reconnects and owns it again.
    let engine = open(state.path());
    let facade = engine.blocking();
    let status = facade.instance_status("svc").unwrap();
    assert_eq!(status.instance.state, LifecycleState::Running, "{status:?}");
    let stopped = facade.stop("svc", None).unwrap();
    assert_eq!(stopped.state, LifecycleState::Stopped);
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < deadline, "the stop ended the child");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn without_a_shim_program_launches_stay_direct() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    hekma_conformance::test_support::ManifestFixture::new("svc", &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("interaction")
        .write(manifest.path());
    let engine = Engine::open(Some(state.path().to_path_buf())).unwrap();
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    facade.start("svc").unwrap();
    let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
    let socket: Option<String> = conn
        .query_row("SELECT shim_socket FROM agent_runtime", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(socket.is_none(), "a direct launch records no Shim");
    facade.stop("svc", None).unwrap();
}

fn manifest(dir: &Path, kind: &str, args: &[&str]) {
    hekma_conformance::test_support::ManifestFixture::new(kind, args)
        .guaranteed_on_all_oses("interaction")
        .guaranteed_on_all_oses("pause")
        .write(dir);
}

fn shim_pid(socket: &str) -> Option<u32> {
    let out = std::process::Command::new("pgrep")
        .args(["-f", socket])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|line| line.trim().parse().ok())
}

fn record(state: &Path) -> (u32, String) {
    let conn = rusqlite::Connection::open(state.join("state.db")).unwrap();
    let (pid, socket): (i64, Option<String>) = conn
        .query_row("SELECT pid, shim_socket FROM agent_runtime", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    (pid as u32, socket.expect("a Shim-launched record"))
}

#[test]
fn pause_resume_and_send_go_through_the_shim() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let state = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    manifest(
        dir.path(),
        "echo",
        &["--echo-stdin", "--linger-ms", "600000"],
    );
    let engine = open(state.path());
    let facade = engine.blocking();
    facade
        .register_with_adapter("echo", &AdapterRef::Manifest(dir.path().to_path_buf()))
        .unwrap();
    facade.start("echo").unwrap();
    facade.send_input("echo", "hello").unwrap();
    let paused = facade.pause("echo").unwrap();
    assert_eq!(paused.state, LifecycleState::Paused);
    let resumed = facade.resume("echo").unwrap();
    assert_eq!(resumed.state, LifecycleState::Running);
    let stopped = facade.stop("echo", None).unwrap();
    assert_eq!(stopped.state, LifecycleState::Stopped);
    // Stopping a stopped instance through its Shim changes nothing.
    assert!(facade.stop("echo", None).is_err());
}

#[test]
fn an_agent_that_exits_on_its_own_is_seen_through_its_shim() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let state = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    manifest(dir.path(), "brief", &["--linger-ms", "800"]);
    let engine = open(state.path());
    let facade = engine.blocking();
    facade
        .register_with_adapter("brief", &AdapterRef::Manifest(dir.path().to_path_buf()))
        .unwrap();
    facade
        .set_restart_policy("brief", hekma_engine::RestartPolicy::Never)
        .unwrap();
    facade.start("brief").unwrap();
    let (pid, _) = record(state.path());
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = facade.instance_status("brief").unwrap();
        if status.instance.state != LifecycleState::Running {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the exit was never seen: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!alive(pid));
}

#[test]
fn a_killed_shim_falls_back_to_the_fingerprint_and_a_stop_still_ends_the_agent() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let state = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    manifest(dir.path(), "orphan", &["--linger-ms", "600000"]);
    let (pid, socket) = {
        let engine = open(state.path());
        let facade = engine.blocking();
        facade
            .register_with_adapter("orphan", &AdapterRef::Manifest(dir.path().to_path_buf()))
            .unwrap();
        facade.start("orphan").unwrap();
        record(state.path())
    };
    let shim = shim_pid(&socket).expect("the Shim process is running");
    std::process::Command::new("kill")
        .args(["-9", &shim.to_string()])
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(shim) {
        assert!(Instant::now() < deadline, "the Shim did not die");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(alive(pid), "the child survives its Shim");
    let engine = open(state.path());
    let facade = engine.blocking();
    let stopped = facade.stop("orphan", None).unwrap();
    assert_eq!(stopped.state, LifecycleState::Stopped);
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < deadline, "the stop ended the child");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_shim_another_connection_holds_is_skipped_by_adoption_and_a_stop_still_ends_it() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let state = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    manifest(dir.path(), "held", &["--linger-ms", "600000"]);
    {
        let engine = open(state.path());
        let facade = engine.blocking();
        facade
            .register_with_adapter("held", &AdapterRef::Manifest(dir.path().to_path_buf()))
            .unwrap();
        facade.start("held").unwrap();
    }
    let (pid, socket) = record(state.path());
    let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
    let launch: String = conn
        .query_row("SELECT shim_launch_id FROM agent_runtime", [], |row| {
            row.get(0)
        })
        .unwrap();
    // Another engine's connection: the Shim's one control connection.
    let key = hekma_engine::shim::secret::host_key(state.path()).unwrap();
    let secret = hekma_engine::shim::secret::connection_secret(&key, &launch);
    let (other, _) =
        hekma_engine::shim::connect(Path::new(&socket), &secret, Duration::from_secs(5)).unwrap();
    // This engine's adoption skips it (busy), and says so.
    let engine = open(state.path());
    let facade = engine.blocking();
    assert!(alive(pid));
    // An explicit stop still acts, through the child's fingerprint.
    let stopped = facade.stop("held", None).unwrap();
    assert_eq!(stopped.state, LifecycleState::Stopped);
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < deadline, "the stop ended the child");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(other);
}
