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
