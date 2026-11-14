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
    let (mut other, _) =
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
    // The holder takes the exit, so the Shim leaves at once (no linger).
    let deadline = Instant::now() + Duration::from_secs(10);
    while other.poll().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the Shim saw the exit");
        std::thread::sleep(Duration::from_millis(50));
    }
    other.ack_exit().unwrap();
    drop(other);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Path::new(&socket).exists() {
        assert!(
            Instant::now() < deadline,
            "the Shim left once its exit was taken"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_rotated_host_key_is_said_and_adoption_falls_back_to_the_fingerprint() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let state = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    manifest(dir.path(), "rotated", &["--linger-ms", "600000"]);
    {
        let engine = open(state.path());
        let facade = engine.blocking();
        facade
            .register_with_adapter("rotated", &AdapterRef::Manifest(dir.path().to_path_buf()))
            .unwrap();
        facade.start("rotated").unwrap();
    }
    let (pid, _) = record(state.path());
    // A new host key: every recorded secret now mismatches.
    std::fs::remove_file(state.path().join("shim-host-key")).unwrap();
    let engine = open(state.path());
    let facade = engine.blocking();
    let status = facade.instance_status("rotated").unwrap();
    assert_eq!(status.instance.state, LifecycleState::Running, "{status:?}");
    facade.stop("rotated", None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < deadline, "the stop ended the child");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_stop_reconnects_to_a_shim_that_was_busy_at_adoption() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let state = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    manifest(dir.path(), "freed", &["--linger-ms", "600000"]);
    let first = open(state.path());
    first
        .blocking()
        .register_with_adapter("freed", &AdapterRef::Manifest(dir.path().to_path_buf()))
        .unwrap();
    first.blocking().start("freed").unwrap();
    let (pid, socket) = record(state.path());
    // A second engine opens while the first holds the Shim: its adoption
    // skips it. Then the first goes away, freeing the Shim.
    let second = open(state.path());
    drop(first);
    let stopped = second.blocking().stop("freed", None).unwrap();
    assert_eq!(stopped.state, LifecycleState::Stopped);
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) || Path::new(&socket).exists() {
        assert!(
            Instant::now() < deadline,
            "the stop went through the Shim, which then left"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Story 22-4: bypass detection end to end. A binding's loopback upstream
/// is on the provider-host list (an Operator-configured local gateway), the
/// agent's tree holds a direct connection to it, and the Run reports it —
/// as informational, since this Run has no wire path (its agent holds its
/// own provider key). Detection never acts on the Run.
#[test]
fn a_shim_reports_a_direct_connection_to_a_listed_host_on_the_run() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let gateway = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let gw = gateway.local_addr().unwrap();
    let state = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let hold = gw.to_string();
    manifest(
        dir.path(),
        "byp",
        &["--linger-ms", "600000", "--hold-tcp", &hold],
    );
    // The binding exists before the Shim engine opens, so its first list
    // already carries the gateway.
    {
        let engine = Engine::open(Some(state.path().to_path_buf())).unwrap();
        engine
            .blocking()
            .create_binding(hekma_engine::BindingSpec {
                name: "gw".into(),
                prefix: "/gw".into(),
                upstream: format!("http://{gw}/v1"),
                labels: Vec::new(),
                custodied_key: None,
                identity_headers: None,
            })
            .unwrap();
    }
    let engine = open(state.path());
    let facade = engine.blocking();
    facade
        .register_with_adapter("byp", &AdapterRef::Manifest(dir.path().to_path_buf()))
        .unwrap();
    facade.start("byp").unwrap();
    let run_id = facade.instance_run("byp").unwrap().record.run_id;
    let deadline = Instant::now() + Duration::from_secs(20);
    let bypass = loop {
        let view = facade.run(run_id.as_str()).unwrap();
        let bypass = view
            .bypass
            .clone()
            .expect("a process-owning Run has a slice");
        if !bypass.hosts.is_empty() {
            assert!(
                view.record.anomalies.is_empty(),
                "informational: no Anomaly"
            );
            assert_eq!(view.record.status, hekma_engine::RunStatus::Active);
            break bypass;
        }
        assert!(Instant::now() < deadline, "never reported: {bypass:?}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(bypass.detection, "active");
    assert_eq!(bypass.hosts, vec!["127.0.0.1".to_string()]);
    assert_eq!(bypass.informational.as_deref(), Some("no-wire"));
    let status = facade.instance_status("byp").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Running,
        "never stopped"
    );
    facade.stop("byp", None).unwrap();
}
