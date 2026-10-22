//! Regression tests for the LIVE-OWNER supervision boundary: an ATTACHED
//! (session-supervised) agent held by a LIVE engine process must be untouchable
//! by a second engine in another process — a benign pass (the `hekma agent
//! list` shape: open → fleet → drop) must NOT stop it, and an explicit `stop`
//! from the second engine must REFUSE honestly instead of silently settling
//! the row while the process keeps running under its real supervisor.
//!
//! The bug this file pins: `Engine::open`'s orphan adoption re-held ANY live
//! write-ahead record — including one whose supervising engine was still
//! ALIVE in another process — with an armed (kill-on-drop) handle, so the
//! first benign `hekma` command after the supervisor started killed the
//! agent at its own engine exit (reproduced with `hekma serve` supervising
//! an attached agent and `hekma agent list` stopping it).
//!
//! Harness: the same re-exec subprocess shape as the adoption suite —
//! "engine 1" runs in a separate child process (a re-exec of this test
//! binary), starts an ATTACHED agent, and then PARKS on stdin so it stays
//! alive and supervising while the parent drives engine 2. The parent
//! releases engine 1 by writing one line to its stdin; engine 1 then stops
//! its agent cleanly and exits. NO OS-cfg here (the gate allowlists only
//! `backends/`); the POSIX liveness legs branch on the runtime OS id and
//! runtime-skip on Windows (Job-Object kill-on-close gives a different, OS-
//! correct NFR-1 shape there — the windows matrix legs carry it).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hekma_conformance::test_support::ManifestFixture;
use hekma_engine::{AdapterRef, Engine, LifecycleState};

/// Whether a pid is alive (`kill -0`, zombie-discounted) — the adoption
/// suite's helper shape: NO OS-cfg (branch on the runtime OS id and shell
/// out); `/proc` reads no-op off Linux.
fn pid_alive(pid: u32) -> bool {
    match hekma_engine::OsId::current() {
        hekma_engine::OsId::Windows => {
            let out = Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output();
            match out {
                Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()),
                Err(_) => false,
            }
        }
        _ => {
            let exists = Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .stdout(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            exists && !proc_pid_is_zombie(pid)
        }
    }
}

fn proc_pid_is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, rest)| rest.split_whitespace().next() == Some("Z"))
        })
        .unwrap_or(false)
}

/// The agent.log path inside an instance's Agent Home.
fn agent_log_path(base: &Path, name: &str) -> PathBuf {
    base.join("agents")
        .join(name)
        .join("logs")
        .join("agent.log")
}

/// Read the pid the fake_agent announced (`ready pid=<n>`) from its log.
fn wait_for_agent_pid(agent_log: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(contents) = std::fs::read_to_string(agent_log) {
            if let Some(line) = contents.lines().find(|l| l.contains("ready pid=")) {
                if let Some(idx) = line.find("pid=") {
                    if let Ok(pid) = line[idx + 4..].trim().parse::<u32>() {
                        return pid;
                    }
                }
            }
        }
        assert!(Instant::now() < deadline, "agent pid never announced");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Linux AND running under CI (GitHub sets `CI`), x86-64 only — the adoption
/// suite's #109 wedge scope: the heavy process-spawning shapes that deadlocked
/// on the x86-64 ubuntu runner. Mirrored here (this file is the same class of
/// subprocess harness); arm64 Linux, macOS, Windows, and every local run
/// execute affirmatively.
fn is_linux_ci() -> bool {
    hekma_engine::OsId::current() == hekma_engine::OsId::Linux
        && std::env::var_os("CI").is_some()
        && std::env::consts::ARCH == "x86_64"
}

/// The re-exec entry for the LIVE "engine 1" (see module docs). When
/// `HEKMA_LIVE_OWNER_STATE` is unset this is a trivial pass (it runs as a
/// normal test in the parent binary too). When set, it registers + starts an
/// ATTACHED `svc`, then parks reading stdin until the parent releases it,
/// then stops the agent cleanly and exits.
#[test]
fn live_owner_helper_subprocess() {
    let Ok(state) = std::env::var("HEKMA_LIVE_OWNER_STATE") else {
        return; // normal in-process invocation: nothing to do.
    };
    let state = PathBuf::from(state);
    let manifest = tempfile::tempdir().expect("manifest dir");
    ManifestFixture::new("livekind", &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("pause")
        .guaranteed_on_all_oses("interaction")
        .write(manifest.path());

    let engine = Engine::open(Some(state)).expect("engine1 open");
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .expect("register");
    facade.start("svc").expect("attached start");
    // Announce readiness, then park: hold the engine (and the attached,
    // armed handle) open until the parent says go — THIS process being alive
    // is the whole fixture.
    println!("LIVE_OWNER_PARKED");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    // Released: stop cleanly (record settled, process gone), then exit.
    // Tolerant by design: a test may have stopped svc from a SECOND engine
    // in the meantime (the on-demand stop), in which case this stop is the
    // uniform invalid-transition no-op — either way the exit is clean.
    let _ = facade.stop("svc", Some(Duration::from_secs(5)));
    hekma_conformance::exit_helper_subprocess(0);
}

/// Spawn the live engine-1 subprocess over `state`; returns the child (its
/// stdin is how the parent releases it later).
fn spawn_live_engine1(state: &Path) -> std::process::Child {
    let exe = std::env::current_exe().expect("test exe");
    Command::new(exe)
        .args(["--exact", "live_owner_helper_subprocess", "--nocapture"])
        .env("HEKMA_LIVE_OWNER_STATE", state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn live engine-1 helper")
}

/// The full fixture: engine 1 (subprocess) supervising an attached `svc`,
/// with the agent's pid known and the child confirmed PARKED (its start is
/// fully committed — record + `running` row — and it now holds the engine
/// open on stdin). Callers MUST release the child (write stdin, wait)
/// before dropping the TempDirs.
fn live_owner_fixture() -> (tempfile::TempDir, std::process::Child, u32) {
    let state = tempfile::tempdir().expect("state dir");
    let base = state.path().to_path_buf();
    let mut child = spawn_live_engine1(&base);
    let pid = wait_for_agent_pid(&agent_log_path(&base, "svc"));
    assert!(pid_alive(pid), "engine 1's attached agent starts alive");
    // Deterministic sync: the child announces PARKED after its start fully
    // commits. libtest writes its own banner lines to this same piped stdout
    // (a leading blank line, "running 1 test", …), so read LINES UNTIL the
    // marker — never assume it is the first (deliberately NOT probed by
    // opening engines here — a concurrent engine open during another
    // engine's in-flight start is exactly the interference this suite
    // exists to police, and the parked handshake keeps the fixture immune).
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stdout = std::io::BufReader::new(child.stdout.take().expect("engine1 stdout"));
    let mut announced = String::new();
    loop {
        announced.clear();
        let n = std::io::BufRead::read_line(&mut stdout, &mut announced)
            .expect("read the PARKED announcement");
        assert!(n > 0, "engine1 exited before parking");
        if announced.trim() == "LIVE_OWNER_PARKED" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "engine1 never parked (last stdout line: {announced:?})"
        );
    }
    drop(stdout);
    (state, child, pid)
}

/// Release engine 1 (clean stop + exit) and prove the agent died with it.
fn release_engine1(child: &mut std::process::Child, pid: u32) {
    child
        .stdin
        .as_mut()
        .expect("engine1 stdin")
        .write_all(b"go\n")
        .expect("release engine1");
    let status = child.wait().expect("engine1 exit");
    assert!(status.success(), "engine1 helper failed: {status}");
    let deadline = Instant::now() + Duration::from_secs(8);
    while pid_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "engine 1's clean stop must end its agent (pid {pid} still alive)"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn a_benign_second_engine_does_not_stop_a_live_owners_attached_agent() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows || is_linux_ci() {
        // POSIX `kill`-based liveness probes + the #109 subprocess-wedge
        // scope (the adoption suite's pointer comment); the windows matrix
        // and non-x86 legs carry the shape.
        return;
    }
    let (state, mut child, pid) = live_owner_fixture();
    let base = state.path().to_path_buf();

    // THE REPRO (the `hekma agent list` shape): a SECOND engine opens over
    // the same state dir, reads the fleet, and drops.
    {
        let engine2 = Engine::open(Some(base.clone())).expect("engine2 open");
        let fleet = engine2.blocking().fleet().expect("engine2 fleet");
        let entry = fleet
            .iter()
            .find(|e| e.name.as_str() == "svc")
            .expect("the live attached agent is listed");
        assert_eq!(entry.state, LifecycleState::Running);
    } // engine2 drops — nothing it did may stop another engine's agent.

    assert!(
        pid_alive(pid),
        "a benign command's engine exit must NOT stop an attached agent a \
         LIVE engine (pid {}) is supervising — the kill-on-drop teardown of a \
         re-adopted handle is exactly the reported bug",
        child.id()
    );

    release_engine1(&mut child, pid);
}

#[test]
fn a_stop_from_a_second_engine_still_terminates_the_agent_on_demand() {
    // The stop-side counterpart of the boundary: benign commands must not
    // touch a live owner's agent, but an EXPLICIT `stop` still means stop —
    // the second engine re-acquires the process on demand by its spawn-record
    // fingerprint and runs the REAL ladder (stop semantics unchanged from
    // before the boundary: the terminal state is true, never a phantom
    // `stopped` row over a live process and never a silent no-op).
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows || is_linux_ci() {
        return; // same scope as the benign leg above.
    }
    let (state, mut child, pid) = live_owner_fixture();
    let base = state.path().to_path_buf();

    // An explicit stop from the second engine (which holds no handle — its
    // open skipped the live owner's record) must TERMINATE the agent.
    let engine3 = Engine::open(Some(base.clone())).expect("engine3 open");
    let stopped = engine3
        .blocking()
        .stop("svc", Some(Duration::from_secs(5)))
        .expect("an explicit stop from a second engine terminates the agent");
    assert_eq!(stopped.state, LifecycleState::Stopped);
    drop(engine3);

    // The process is really gone, and the durable row says stopped.
    let deadline = Instant::now() + Duration::from_secs(8);
    while pid_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "the on-demand stop must end the agent (pid {pid} still alive)"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    let probe = Engine::open(Some(base.clone())).expect("probe engine open");
    let fleet = probe.blocking().fleet().expect("probe fleet");
    let entry = fleet
        .iter()
        .find(|e| e.name.as_str() == "svc")
        .expect("svc row present");
    assert_eq!(
        entry.state,
        LifecycleState::Stopped,
        "the on-demand stop settles the row truthfully"
    );
    drop(probe);

    release_engine1(&mut child, pid);
}
