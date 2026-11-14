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

/// Story 24-2: `shutdown_for_handoff` releases a Shim-held agent (no stop),
/// and the next engine reconnects to it.
#[test]
fn a_handoff_shutdown_releases_shim_agents_and_the_next_engine_takes_them_back() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let state = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    manifest(dir.path(), "kept", &["--linger-ms", "600000"]);
    let pid = {
        let engine = open(state.path());
        let facade = engine.blocking();
        facade
            .register_with_adapter("kept", &AdapterRef::Manifest(dir.path().to_path_buf()))
            .unwrap();
        facade.start("kept").unwrap();
        let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
        let pid: i64 = conn
            .query_row("SELECT pid FROM agent_runtime", [], |row| row.get(0))
            .unwrap();
        let pid = pid as u32;
        let handed = facade.shutdown_for_handoff(Duration::from_secs(5), None);
        assert_eq!(handed.released, vec!["kept".to_string()]);
        assert!(handed.outcomes.is_empty(), "nothing was held directly");
        pid
    };
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(pid), "released, not stopped");
    let engine = open(state.path());
    let facade = engine.blocking();
    let status = facade.instance_status("kept").unwrap();
    assert_eq!(status.instance.state, LifecycleState::Running, "{status:?}");
    // A second handoff shutdown with nothing held stops nothing either.
    facade.stop("kept", None).unwrap();
    let handed = facade.shutdown_for_handoff(Duration::from_secs(5), None);
    assert!(handed.released.is_empty() && handed.outcomes.is_empty());
}

/// Story 24-2: the upgrade shutdown keeps the metering proxy's socket open
/// and hands it back; the next engine serves on that same socket, and the
/// released agent is adopted even while the first engine is still alive.
#[test]
fn an_upgrade_shutdown_hands_back_the_proxy_socket_and_the_next_engine_serves_on_it() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let state = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    manifest(dir.path(), "kept", &["--linger-ms", "600000"]);
    let first = open(state.path());
    let facade = first.blocking();
    facade
        .register_with_adapter("kept", &AdapterRef::Manifest(dir.path().to_path_buf()))
        .unwrap();
    facade.start("kept").unwrap();
    let port = first.metering_proxy().expect("a proxy").port;
    let (handed, listener) = facade.shutdown_for_upgrade(Duration::from_secs(5), None);
    assert_eq!(handed.released, vec!["kept".to_string()]);
    let listener = listener.expect("the socket is handed back");
    assert_eq!(listener.local_addr().unwrap().port(), port);
    // Still open: a connection waits in the backlog instead of being refused.
    let early = std::net::TcpStream::connect(("127.0.0.1", port));
    assert!(early.is_ok(), "the socket stayed open across the hand-over");
    // The second engine opens BEFORE the first is dropped.
    let second = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            shim_exe: Some(hekma_conformance::test_support::shim_host_bin()),
            metering_proxy: hekma_engine::MeteringProxyOptions {
                inherited_listener: hekma_engine::InheritedListener::new(listener),
                ..hekma_engine::MeteringProxyOptions::default()
            },
            ..EngineOptions::default()
        },
    )
    .unwrap();
    assert_eq!(second.metering_proxy().expect("a proxy").port, port);
    let status = second.blocking().instance_status("kept").unwrap();
    assert_eq!(status.instance.state, LifecycleState::Running, "{status:?}");
    // The second engine holds it: its stop ends the agent.
    second.blocking().stop("kept", None).unwrap();
    drop(first);
}

fn open_as_host(state: &Path, posture: hekma_engine::HandoffPosture) -> Engine {
    Engine::open_with_options(
        Some(state.to_path_buf()),
        EngineOptions {
            shim_exe: Some(hekma_conformance::test_support::shim_host_bin()),
            host_posture: Some(posture),
            ..EngineOptions::default()
        },
    )
    .unwrap()
}

/// Story 24-3: the open reads the previous owner's record before adopting:
/// none → `Unknown`; an owner that chose off and is gone → its orphans are
/// listed, and `clean_up_after_crash` stops each the ordinary way, ending
/// its Run `engine-crash`.
#[test]
fn the_next_owner_reads_the_previous_posture_and_cleans_up_after_an_off_crash() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    use hekma_engine::{HandoffPosture, PreviousHost};
    let state = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    manifest(dir.path(), "orph", &["--linger-ms", "600000"]);
    {
        let first = open_as_host(state.path(), HandoffPosture::Off);
        let recovery = first.host_recovery().expect("a host records and reads");
        assert_eq!(
            recovery.previous,
            PreviousHost::Unknown,
            "nothing recorded yet"
        );
        assert!(recovery.adopted.is_empty());
        let facade = first.blocking();
        facade
            .register_with_adapter("orph", &AdapterRef::Manifest(dir.path().to_path_buf()))
            .unwrap();
        facade.start("orph").unwrap();
        // Dropped without a shutdown: the Shim keeps the agent (a crash).
    }
    let second = open_as_host(state.path(), HandoffPosture::Unset);
    let recovery = second.host_recovery().unwrap();
    assert_eq!(recovery.previous, PreviousHost::Gone { handoff: false });
    assert_eq!(recovery.adopted, vec!["orph".to_string()]);
    let run_id = second
        .blocking()
        .instance_run("orph")
        .unwrap()
        .record
        .run_id;
    let stopped = second
        .blocking()
        .clean_up_after_crash("orph", Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(stopped.state, LifecycleState::Stopped);
    let run = second.blocking().run(run_id.as_str()).unwrap();
    assert_eq!(run.record.end_cause.as_deref(), Some("engine-crash"));
    // The one summary: off, one cleaned, none kept.
    let summary = second.blocking().commit_recovery(1).unwrap().unwrap();
    assert_eq!(summary.posture, hekma_engine::RecoveryPosture::Off);
    assert_eq!((summary.runs_cleaned, summary.runs_adopted), (1, 0));
    drop(second);
    // The second owner recorded `unset`: a third sees an unknown wish.
    let third = open_as_host(state.path(), HandoffPosture::On);
    assert_eq!(
        third.host_recovery().unwrap().previous,
        PreviousHost::Unknown
    );
    // An engine that owns no data path records and reads nothing.
    let plain = Engine::open(Some(state.path().to_path_buf())).unwrap();
    assert!(plain.host_recovery().is_none());
    assert!(plain.blocking().commit_recovery(0).unwrap().is_none());
}

/// Story 24-3: each host start commits one `recovery` event summing up its
/// reconcile — the previous mode, the agents kept, and the Shims found
/// gone — on the bus and in the outbox; a second restart sums up nothing.
#[test]
fn each_host_start_commits_one_recovery_event_that_sums_up_its_reconcile() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    use hekma_engine::{EngineEvent, HandoffPosture, RecoveryPosture};
    let state = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    manifest(dir.path(), "kept", &["--linger-ms", "600000"]);
    {
        let first = open_as_host(state.path(), HandoffPosture::On);
        let facade = first.blocking();
        facade
            .register_with_adapter("kept", &AdapterRef::Manifest(dir.path().to_path_buf()))
            .unwrap();
        facade.start("kept").unwrap();
        // Dropped without a shutdown: a crash.
    }
    // The Shim dies while no host holds it; the agent lives on.
    let (pid, socket) = record(state.path());
    kill9(shim_pid(&socket).expect("the Shim runs"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while shim_pid(&socket).is_some() {
        assert!(Instant::now() < deadline, "the Shim never died");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(alive(pid));
    let second = open_as_host(state.path(), HandoffPosture::Unset);
    assert_eq!(
        second.host_recovery().unwrap().shims_lost,
        vec!["kept".to_string()]
    );
    let mut events = second.subscribe();
    let summary = second.blocking().commit_recovery(0).unwrap().unwrap();
    assert_eq!(summary.posture, RecoveryPosture::On);
    assert_eq!(
        (
            summary.runs_adopted,
            summary.runs_cleaned,
            summary.shims_lost,
            summary.calls_closed_crash,
            summary.calls_closed_recovered,
        ),
        (1, 0, 1, 0, 0)
    );
    let published = loop {
        match events.try_recv() {
            Ok(EngineEvent::Recovery(event)) => break event,
            Ok(_) => continue,
            Err(err) => panic!("no recovery event on the bus: {err:?}"),
        }
    };
    assert_eq!(published, summary);
    second
        .blocking()
        .stop("kept", Some(Duration::from_secs(5)))
        .unwrap();
    drop(second);
    // The outbox kept it.
    let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
    let kept: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM outbox WHERE event LIKE '%\"kind\":\"recovery\"%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1);
    // Nothing left behind: an unknown wish, and zero counts.
    let third = open_as_host(state.path(), HandoffPosture::Unset);
    let summary = third.blocking().commit_recovery(0).unwrap().unwrap();
    assert_eq!(summary.posture, RecoveryPosture::Unknown);
    assert_eq!(
        (
            summary.runs_adopted,
            summary.shims_lost,
            summary.runs_cleaned
        ),
        (0, 0, 0)
    );
}

/// A diagnostic sink capturing into a shared buffer.
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn kill9(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}

/// Story 24-3 (T4): a Shim that dies under a live agent does not make the
/// agent look crashed (no relaunch beside it): the engine re-holds it
/// directly, says so, and its stop still ends it. A Shim that dies with
/// its agent is an exit whose code is unavailable.
#[test]
fn a_dead_shim_degrades_to_direct_supervision_and_never_relaunches_its_live_agent() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    let state = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    manifest(dir.path(), "lone", &["--linger-ms", "600000"]);
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let sink: hekma_engine::DiagnosticSink = std::sync::Arc::new(std::sync::Mutex::new(Box::new(
        Capture(std::sync::Arc::clone(&captured)),
    )
        as Box<dyn std::io::Write + Send>));
    let lines = || String::from_utf8_lossy(&captured.lock().unwrap()).into_owned();
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            shim_exe: Some(hekma_conformance::test_support::shim_host_bin()),
            diagnostics: Some(sink),
            ..EngineOptions::default()
        },
    )
    .unwrap();
    let facade = engine.blocking();
    facade
        .register_with_adapter("lone", &AdapterRef::Manifest(dir.path().to_path_buf()))
        .unwrap();
    facade.start("lone").unwrap();
    let (pid, socket) = record(state.path());
    kill9(shim_pid(&socket).expect("the Shim runs"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !lines().contains("its Shim is gone") {
        assert!(Instant::now() < deadline, "never noticed: {}", lines());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(alive(pid), "the agent outlives its Shim");
    // SM-7: a Shim's death stops, pauses and blocks nothing.
    let actions = hekma_conformance::sm7::audit_state_dir(state.path());
    assert!(actions.is_empty(), "{actions:?}");
    let status = facade.instance_status("lone").unwrap();
    assert_eq!(status.instance.state, LifecycleState::Running, "{status:?}");
    assert_eq!(status.restart_count, 0, "never relaunched beside itself");
    facade.stop("lone", Some(Duration::from_secs(5))).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < deadline, "the direct stop ended it");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Both gone: an exit, its code unavailable.
    facade.start("lone").unwrap();
    let (pid, socket) = record(state.path());
    let shim = shim_pid(&socket).expect("the Shim runs");
    kill9(pid);
    kill9(shim);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = facade.instance_status("lone").unwrap().instance.state;
        if state != LifecycleState::Running {
            break;
        }
        assert!(Instant::now() < deadline, "the exit was never seen");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Story 24-3 (the 18-1 deferral): a row a dead host left `starting` is
/// settled at adoption, never left `starting` — `running` with
/// `adopted-unconfirmed` when a readiness probe was declared (its answer
/// died with the host), plain `adapter-ready` when none was.
#[test]
fn a_row_left_starting_is_settled_at_adoption_and_names_an_unanswered_probe() {
    if std::env::consts::FAMILY != "unix" {
        return;
    }
    use hekma_engine::TransitionCause;
    let state = TempDir::new().unwrap();
    let gated = TempDir::new().unwrap();
    hekma_conformance::test_support::ManifestFixture::new(
        "gated",
        &[
            "--ready-line",
            "readiness-line-z9:50",
            "--linger-ms",
            "600000",
        ],
    )
    .guaranteed_on_all_oses("interaction")
    .readiness("kind = \"log-contains\"\ntimeout_ms = 8000\nline = \"readiness-line-z9\"")
    .write(gated.path());
    let plain = TempDir::new().unwrap();
    manifest(plain.path(), "plain", &["--linger-ms", "600000"]);
    {
        let first = open(state.path());
        let facade = first.blocking();
        facade
            .register_with_adapter("gated", &AdapterRef::Manifest(gated.path().to_path_buf()))
            .unwrap();
        facade
            .register_with_adapter("plain", &AdapterRef::Manifest(plain.path().to_path_buf()))
            .unwrap();
        facade.start("gated").unwrap();
        facade.start("plain").unwrap();
    }
    // The host "died mid-start": both rows read `starting`.
    let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
    conn.execute("UPDATE agent_instances SET state = 'starting'", [])
        .unwrap();
    let second = open(state.path());
    let facade = second.blocking();
    for (name, expected) in [("gated", "adopted-unconfirmed"), ("plain", "adapter-ready")] {
        let status = facade.instance_status(name).unwrap();
        assert_eq!(status.instance.state, LifecycleState::Running, "{name}");
        let last = facade.transition_events(name).unwrap().pop().unwrap();
        assert_eq!(last.prior_state, LifecycleState::Starting, "{name}");
        let kind = serde_json::to_value(&last.cause).unwrap()["kind"].clone();
        assert_eq!(kind, expected, "{name}: {:?}", last.cause);
        if let TransitionCause::AdoptedUnconfirmed { detail } = &last.cause {
            assert!(detail.contains("readiness-line-z9"), "{detail}");
        }
        facade.stop(name, Some(Duration::from_secs(5))).unwrap();
    }
}
