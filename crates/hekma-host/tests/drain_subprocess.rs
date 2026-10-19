//! Integration tests for the host's shutdown drain against REAL agent
//! processes (story 17-2): a cooperative agent stops gracefully within the
//! window; a SIGTERM-ignoring agent escalates (forced) after the window with
//! the elapsed time pinned; and a full [`serve_with_signal`] run drains a
//! DETACHED running agent it adopted at open. The engine owns every stop
//! (the host never signals a process itself, AD-20) — these tests prove the
//! host drives that ladder faithfully and surfaces each outcome.
//!
//! POSIX shapes (`sh`, `trap`, `kill`) are Unix-only: the Windows legs
//! runtime-skip via the data-driven OS id (NO `#[cfg]` — this file is
//! outside the backends/ allowlist), exactly the `lifecycle.rs` pattern.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hekma_conformance::test_support::ManifestFixture;
use hekma_engine::{AdapterRef, Engine};
use std::io::Write;

/// A sink that records every notice into a shared buffer the test can
/// assert on.
#[derive(Clone)]
struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl SharedSink {
    fn new() -> (Self, hekma_engine::DiagnosticSink) {
        let shared = Self(Arc::new(Mutex::new(Vec::new())));
        let sink: hekma_engine::DiagnosticSink = Arc::new(Mutex::new(Box::new(shared.clone())));
        (shared, sink)
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

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
                .stderr(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
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

fn wait_until_gone(pid: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while pid_alive(pid) {
        assert!(Instant::now() < deadline, "{what} (pid {pid} still alive)");
        std::thread::sleep(Duration::from_millis(30));
    }
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

/// A fresh state root + manifest dir + a registered+running cooperative
/// fake_agent (`--linger-ms` long enough to outlive the drain window).
/// Returns (state, manifest_dir, engine, agent_pid).
fn running_fake_agent(name: &str) -> (PathBuf, tempfile::TempDir, Arc<Engine>, u32) {
    let state = tempfile::tempdir().unwrap();
    let manifest = tempfile::tempdir().unwrap();
    let state_path = state.path().to_path_buf();
    ManifestFixture::new(name, &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("pause")
        .guaranteed_on_all_oses("interaction")
        .write(manifest.path());
    let engine = Arc::new(Engine::open(Some(state_path.clone())).expect("engine open"));
    let facade = engine.blocking();
    facade
        .register_with_adapter(name, &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .expect("register");
    facade.start(name).expect("start");
    let pid = wait_for_agent_pid(&agent_log_path(&state_path, name));
    (state_path, manifest, engine, pid)
}

#[test]
fn the_drain_stops_a_cooperative_agent_gracefully_and_surfaces_it() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        // POSIX `kill`-based liveness probes only; Windows drain semantics
        // are covered by the windows matrix on the same engine ladder.
        return;
    }
    let (state_path, _manifest_dir, engine, pid) = running_fake_agent("coop");
    let (sink_handle, sink) = SharedSink::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let outcomes = rt.block_on(hekma_host::drain_and_stop(
        &engine,
        Duration::from_secs(5),
        &sink,
    ));
    assert_eq!(outcomes.len(), 1, "one running agent → one outcome");
    assert_eq!(
        outcomes[0].result,
        hekma_host::StopResult::Graceful,
        "a cooperative agent exits within the window: {outcomes:?}"
    );
    wait_until_gone(pid, "the drained agent must be gone");
    let text = sink_handle.text();
    assert!(
        text.contains("stopping coop") && text.contains("stopped gracefully"),
        "the drain notes each step:\n{text}"
    );
    drop(engine);
    drop(state_path);
}

#[test]
fn the_drain_escalates_a_sigterm_ignoring_agent_after_the_window() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        return; // `sh`/`trap` are POSIX; the windows matrix proves escalation.
    }
    let state = tempfile::tempdir().unwrap();
    let manifest = tempfile::tempdir().unwrap();
    let state_path = state.path().to_path_buf();
    // The lifecycle suite's stubborn agent: IGNORES SIGTERM, so the bounded
    // ladder must run the full window before escalating.
    let body = r#"
contract_version = "1.0.0"

[adapter]
kind = "stubborn"

[lifecycle.start]
exec = "sh"
args = ["-c", "trap '' TERM; sleep 120"]

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#;
    std::fs::write(manifest.path().join("adapter.toml"), body).unwrap();
    let engine = Arc::new(Engine::open(Some(state_path.clone())).unwrap());
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "stubborn",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade.start("stubborn").unwrap();
    // The `sh` agent never announces `ready pid=`; the outcome + the honored
    // window + the engine's committed stop record are the proof (the
    // escalation ladder itself is the engine's, already proven by
    // lifecycle.rs — here the HOST drives it).
    let (sink_handle, sink) = SharedSink::new();
    let window = Duration::from_millis(400);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let started = Instant::now();
    let outcomes = rt.block_on(hekma_host::drain_and_stop(&engine, window, &sink));
    let elapsed = started.elapsed();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes[0].result,
        hekma_host::StopResult::Forced,
        "a SIGTERM-ignoring agent must escalate: {outcomes:?}"
    );
    // THE WINDOW IS HONORED: the ladder waited at least the full window
    // before escalating (the host never kills early — the AC).
    assert!(
        elapsed >= window,
        "the drain must honor the full window before escalating, took {elapsed:?}"
    );
    // ...and it did not hang: bounded (2s ceiling for a 400ms window).
    assert!(
        elapsed < Duration::from_secs(4),
        "the drain must be bounded, took {elapsed:?}"
    );
    let text = sink_handle.text();
    assert!(
        text.contains("force-killed"),
        "the escalation is surfaced:\n{text}"
    );
    drop(engine);
    drop(state_path);
}

#[test]
fn serve_adopts_a_detached_agent_and_drains_it_on_the_supplied_signal() {
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        return; // `kill`-based liveness probes only.
    }
    let state = tempfile::tempdir().unwrap();
    let manifest = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    ManifestFixture::new("detached", &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("pause")
        .guaranteed_on_all_oses("interaction")
        .write(manifest.path());
    // Engine 1: detached start (the child survives the engine's exit), then
    // a CLEAN drop — the detached child is left running for the host.
    {
        let engine1 = Engine::open(Some(base.clone())).unwrap();
        let facade = engine1.blocking();
        facade
            .register_with_adapter(
                "detached",
                &AdapterRef::Manifest(manifest.path().to_path_buf()),
            )
            .unwrap();
        facade.start_detached("detached").unwrap();
        let pid = wait_for_agent_pid(&agent_log_path(&base, "detached"));
        drop(engine1); // clean exit: the detached child SURVIVES (story 12-1)
        assert!(
            pid_alive(pid),
            "the detached agent survives engine 1's exit"
        );
        // Engine 1 is gone; its lock released with it. The HOST takes over.
        let (sink_handle, sink) = SharedSink::new();
        let options = hekma_host::ServeOptions {
            state_base: Some(base.clone()),
            stop_window: Some(5),
            diagnostics: Some(sink),
            ..hekma_host::ServeOptions::default()
        };
        let result =
            hekma_host::serve_with_signal(options, |_sink| std::future::ready(Ok("SIGTEST")));
        assert!(result.is_ok(), "serve must complete: {result:?}");
        wait_until_gone(pid, "the host's drain must stop the adopted agent");
        let text = sink_handle.text();
        // The adoption + drain + completion are all on the record (AI-18).
        assert!(
            text.contains("adopted from a previous engine"),
            "the adoption is surfaced:\n{text}"
        );
        assert!(
            text.contains("stopping detached") && text.contains("stopped"),
            "the drain outcome is surfaced:\n{text}"
        );
        assert!(
            text.contains("shutdown complete"),
            "completion noted:\n{text}"
        );
    }
}

#[test]
fn serve_exits_zero_even_when_an_agent_forces_the_ladder_to_escalate() {
    // Review finding 13's pin: the exit-0-on-failed/escalated-stop contract
    // through the SERVE path. A SIGTERM-IGNORING agent (the `trap '' TERM`
    // shape) makes the bounded ladder run the full window and force-kill —
    // the outcome is `Forced`, the stream says so honestly, and the host's
    // contract (the drain RAN) is kept: `serve` returns `Ok(())`. A plausible
    // "harden the exit code on any failed stop" regression must ship red.
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        return; // `sh`/`trap`/`kill` are POSIX; the windows matrix runs the same ladder.
    }
    let state = tempfile::tempdir().unwrap();
    let manifest = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    // The stubborn fixture (the escalation test's shape): ignores SIGTERM.
    let body = r#"
contract_version = "1.0.0"

[adapter]
kind = "stubborn"

[lifecycle.start]
exec = "sh"
args = ["-c", "trap '' TERM; sleep 120"]

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#;
    std::fs::write(manifest.path().join("adapter.toml"), body).unwrap();
    {
        let engine1 = Engine::open(Some(base.clone())).unwrap();
        let facade = engine1.blocking();
        facade
            .register_with_adapter(
                "stubborn",
                &AdapterRef::Manifest(manifest.path().to_path_buf()),
            )
            .unwrap();
        facade.start_detached("stubborn").unwrap();
        drop(engine1); // the detached child survives for the host
        let (sink_handle, sink) = SharedSink::new();
        let options = hekma_host::ServeOptions {
            state_base: Some(base.clone()),
            // The minimum legal window (finding 9's bounds): the ladder
            // honors it fully before escalating — the test stays fast.
            stop_window: Some(1),
            diagnostics: Some(sink),
            ..hekma_host::ServeOptions::default()
        };
        let started = Instant::now();
        let result =
            hekma_host::serve_with_signal(options, |_sink| std::future::ready(Ok("SIGTEST")));
        let elapsed = started.elapsed();
        // THE CONTRACT: Ok(()) despite the escalated outcome.
        assert!(
            result.is_ok(),
            "the drain ran; an escalated stop must NOT fail the host: {result:?}"
        );
        // The escalation is on the record (AI-18), and the window was
        // honored before it fired.
        let text = sink_handle.text();
        assert!(
            text.contains("stopping stubborn") && text.contains("force-killed"),
            "the escalated outcome is surfaced:\n{text}"
        );
        assert!(
            elapsed >= Duration::from_secs(1),
            "the window must be honored before the escalation: {elapsed:?}"
        );
        assert!(
            text.contains("shutdown complete (SIGTEST)"),
            "the completion names the signal:\n{text}"
        );
    }
}
