//! Integration tests for story-17-2's engine serve seams (the `metering.listen`
//! fixed port + the child-environment allowlist), driven through the PUBLIC
//! [`EngineOptions`] / `Engine::open_with_options` with the REAL `fake_agent`.
//!
//! * Fixed port: an observed instance's injected `base_url` carries the
//!   HOST-pinned port (the dump proof) and the listener answers connects on
//!   it; a host RESTART re-binds the SAME port.
//! * The AI-46 closure: an ADOPTED observed instance gets a LIVE re-bound
//!   listener on the fixed port (no strand, no stop/start) — and an
//!   ephemeral-port open still keeps the story-11-3 strand diagnostic
//!   byte-for-byte.
//! * Env allowlist: under `child_env_allowlist`, a child's environment is
//!   EXACTLY its declared mappings plus the documented OS-baseline floor
//!   (Unix PATH/HOME — process plumbing, not host leakage); the default
//!   open keeps full inheritance byte-identically.
//!
//! The adoption legs simulate an engine crash with the re-exec helper
//! pattern (`adoption.rs`): the "engine 1" work runs in a child process that
//! exits WITHOUT dropping the engine, so the agent survives re-parented to
//! init. Those legs are skipped on the x86_64 Linux CI runner only
//! (`is_linux_ci`), like their `adoption.rs` templates.

use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hekma_engine::{AdapterRef, Engine, EngineError, EngineOptions};
use tempfile::TempDir;

/// A `Write` adapter delegating into a shared buffer (the diagnostic_sink.rs
/// shape), so the test can read the sink's captured bytes.
#[derive(Clone)]
struct SharedCapture(Arc<Mutex<Vec<u8>>>);

impl Write for SharedCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn make_sink(shared: &Arc<Mutex<Vec<u8>>>) -> hekma_engine::DiagnosticSink {
    Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(shared)))))
}

fn sink_text(shared: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(shared.lock().unwrap().clone()).unwrap()
}

/// Claim an OS-ephemeral loopback port and RELEASE it, so a test can pin a
/// port that is free (the standard fixed-port test pick; the bind race
/// window is the usual accepted test-tradeoff).
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("probe bind");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// Whether a fixed port currently answers a TCP connect.
fn port_answer(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).is_ok()
}

/// The observed-instance manifest (the adoption.rs shape): `engine-observed`
/// metering, the `OPENAI_BASE_URL` mapping the engine's injection targets,
/// plus the caller's fake_agent args (the `--dump` vehicle).
fn write_observed_manifest(dir: &Path, kind: &str, args: &[&str]) {
    let bin = hekma_conformance::fake_agent_bin();
    let args_toml = args
        .iter()
        .map(|a| format!("{a:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
        r#"
contract_version = "1.0.0"

[adapter]
kind = "{kind}"

[lifecycle.start]
exec = {exec:?}
args = [{args_toml}]

[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "best-effort"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "engine-observed"

[config."metering.base_url"]
env = "OPENAI_BASE_URL"
"#,
        exec = bin.to_string_lossy(),
    );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

/// Poll until the dump file exists and contains `needle`; returns its text.
fn poll_dump_for(dump: &Path, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(dump) {
            if text.contains(needle) {
                return text;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the agent never wrote its dump at {}: expected {needle:?}; current content: {:?}",
            dump.display(),
            std::fs::read_to_string(dump).unwrap_or_else(|_| "<absent>".to_string())
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether a pid is alive (`kill -0`, zombie-discounted; tasklist on
/// Windows) — NO OS-cfg, the adoption.rs shape.
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

/// Kill a process out-of-band (adoption.rs shape; the agent is a session
/// leader re-parented to init, so init reaps it).
fn kill_pid(pid: u32) {
    match hekma_engine::OsId::current() {
        hekma_engine::OsId::Windows => {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .status();
        }
        _ => {
            for target in [format!("-{pid}"), pid.to_string()] {
                let _ = Command::new("kill")
                    .args(["-KILL", &target])
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
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

/// Linux AND running under CI (GitHub sets `CI`) — the adoption.rs skip for
/// the re-exec subprocess legs that deadlock on the x86-64 ubuntu runner.
fn is_linux_ci() -> bool {
    hekma_engine::OsId::current() == hekma_engine::OsId::Linux
        && std::env::var_os("CI").is_some()
        && std::env::consts::ARCH == "x86_64"
}

/// Run the "engine 1" re-exec helper (adoption.rs shape): a child process of
/// THIS test binary that opens an engine with the FIXED port, starts the
/// observed instance, and exits WITHOUT dropping the engine (crash
/// semantics — the agent survives).
fn run_engine1_fixed_port(state: &Path, manifest: &Path, port: u16) {
    let exe = std::env::current_exe().expect("test exe");
    let status = Command::new(exe)
        .args(["--exact", "serve_helper_subprocess", "--nocapture"])
        .env("KTESIO_SERVE_HELPER", "observed_fixed_port")
        .env("KTESIO_SERVE_STATE", state)
        .env("KTESIO_SERVE_MANIFEST", manifest)
        .env("KTESIO_SERVE_PORT", port.to_string())
        .env("LLVM_PROFILE_FILE", hekma_conformance::helper_profile_env())
        .status()
        .expect("run serve-1 helper subprocess");
    assert!(
        status.success(),
        "serve-1 helper subprocess failed: {status}"
    );
}

/// The re-exec entry: when `KTESIO_SERVE_HELPER` is unset this is a trivial
/// pass (a normal test in the parent binary). When set: engine 1 opens with
/// the fixed port + the observed instance starts, then the process exits
/// WITHOUT dropping the engine — modelling the crashed host whose observed
/// child survives, pointing at the now-dead fixed port.
#[test]
fn serve_helper_subprocess() {
    let Ok(mode) = std::env::var("KTESIO_SERVE_HELPER") else {
        return; // normal in-process invocation: nothing to do.
    };
    assert_eq!(mode, "observed_fixed_port", "unknown helper mode");
    let state = PathBuf::from(std::env::var("KTESIO_SERVE_STATE").unwrap());
    let manifest = PathBuf::from(std::env::var("KTESIO_SERVE_MANIFEST").unwrap());
    let port: u16 = std::env::var("KTESIO_SERVE_PORT").unwrap().parse().unwrap();
    let engine = Engine::open_with_options(
        Some(state.clone()),
        EngineOptions {
            metering_listen_port: port,
            ..EngineOptions::default()
        },
    )
    .expect("engine1 open with fixed port");
    let facade = engine.blocking();
    facade
        .register_with_adapter("observed", &AdapterRef::Manifest(manifest))
        .unwrap();
    facade
        .set_config(
            "observed",
            "metering.upstream_base_url",
            "http://127.0.0.1:9/",
        )
        .unwrap();
    facade.start("observed").unwrap();
    // Crash: exit without dropping the engine. The child survives (its own
    // session), its injected base_url points at the now-dead port.
    hekma_conformance::exit_helper_subprocess(0);
}

#[test]
fn a_fixed_metering_port_binds_and_injects_that_port() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let dump = state.path().join("observed.dump");
    write_observed_manifest(
        manifest.path(),
        "observedkind",
        &[
            "--linger-ms",
            "600000",
            "--dump",
            &dump.display().to_string(),
        ],
    );
    let port = free_port();
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            metering_listen_port: port,
            ..EngineOptions::default()
        },
    )
    .expect("open with fixed port");
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "observed",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_config(
            "observed",
            "metering.upstream_base_url",
            "http://127.0.0.1:9/",
        )
        .unwrap();
    facade.start("observed").unwrap();
    // The INJECTED base_url names the FIXED port (the agent-observable seam).
    let text = poll_dump_for(
        &dump,
        &format!("env=OPENAI_BASE_URL=http://127.0.0.1:{port}"),
    );
    assert!(
        text.contains(&format!("env=OPENAI_BASE_URL=http://127.0.0.1:{port}")),
        "the injected base_url must carry the pinned port"
    );
    // The listener ANSWERS on that port (loopback, live accept loop).
    assert!(
        port_answer(port),
        "the fixed-port listener must accept connects"
    );
    facade
        .stop("observed", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn a_host_restart_rebinds_the_same_fixed_port() {
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let dump = state.path().join("restart.dump");
    write_observed_manifest(
        manifest.path(),
        "observedkind",
        &[
            "--linger-ms",
            "600000",
            "--dump",
            &dump.display().to_string(),
        ],
    );
    let port = free_port();
    let base = state.path().to_path_buf();
    // Generation 1: start, observe the pinned port, stop, clean exit.
    {
        let engine1 = Engine::open_with_options(
            Some(base.clone()),
            EngineOptions {
                metering_listen_port: port,
                ..EngineOptions::default()
            },
        )
        .unwrap();
        let facade = engine1.blocking();
        facade
            .register_with_adapter(
                "observed",
                &AdapterRef::Manifest(manifest.path().to_path_buf()),
            )
            .unwrap();
        facade
            .set_config(
                "observed",
                "metering.upstream_base_url",
                "http://127.0.0.1:9/",
            )
            .unwrap();
        facade.start("observed").unwrap();
        poll_dump_for(
            &dump,
            &format!("env=OPENAI_BASE_URL=http://127.0.0.1:{port}"),
        );
        facade
            .stop("observed", Some(Duration::from_secs(5)))
            .unwrap();
        // engine1 dropped here: its listener released the port.
    }
    assert!(
        !port_answer(port),
        "generation 1's listener must be gone before the restart"
    );
    // Generation 2 (the host restart): the SAME port re-binds and injects.
    // (The dump is removed so gen 2's own write is what the poll sees.)
    std::fs::remove_file(&dump).ok();
    let engine2 = Engine::open_with_options(
        Some(base),
        EngineOptions {
            metering_listen_port: port,
            ..EngineOptions::default()
        },
    )
    .unwrap();
    let facade = engine2.blocking();
    facade.start("observed").unwrap();
    poll_dump_for(
        &dump,
        &format!("env=OPENAI_BASE_URL=http://127.0.0.1:{port}"),
    );
    assert!(
        port_answer(port),
        "the restarted host's listener must answer the SAME port"
    );
    facade
        .stop("observed", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn an_adopted_observed_instance_is_rebound_live_on_the_fixed_port() {
    if is_linux_ci() {
        // The re-exec legs deadlock on the x86-64 ubuntu runner only (#109).
        return;
    }
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let dump = state.path().join("adopted.dump");
    write_observed_manifest(
        manifest.path(),
        "observedkind",
        &[
            "--linger-ms",
            "600000",
            "--dump",
            &dump.display().to_string(),
        ],
    );
    let port = free_port();
    // Engine 1 (the subprocess): opens with the fixed port, starts the
    // observed agent, exits WITHOUT dropping — the agent survives pointing
    // at the now-dead port.
    run_engine1_fixed_port(state.path(), manifest.path(), port);
    // The agent dumped its base_url BEFORE the crash: the pinned port.
    let text = poll_dump_for(
        &dump,
        &format!("env=OPENAI_BASE_URL=http://127.0.0.1:{port}"),
    );
    assert!(text.contains("env=OPENAI_BASE_URL="));
    let pid = wait_for_agent_pid(&agent_log_path(state.path(), "observed"));
    // The NFR-1 crash-survival vehicle is Unix-only: on Windows the engine's
    // Job-Object kill-on-close takes the observed child the moment engine1's
    // process exits (the documented no-orphan guarantee — adoption.rs's
    // Windows-gated NFR-1 proofs and its Windows-positive mirror), so the
    // live-orphan window this test drives cannot exist there. Pin the
    // documented Windows outcome (no leaked child) and stop; the OS-uniform
    // half of the fixed-port re-bind proof is pinned in-process by the
    // restart sibling (`a_host_restart_rebinds_the_same_fixed_port`), and
    // this crash-shaped adoption leg runs where an orphan can survive.
    // Runtime branch per the boundary gate's data-driven OS rule.
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        wait_until_gone(
            pid,
            "NFR-1: the crashed engine1 must not leak the observed child on Windows",
        );
        return;
    }
    assert!(pid_alive(pid), "the orphan survived the crash");
    assert!(
        !port_answer(port),
        "between the engines, the fixed port is dead (the strand window)"
    );
    // Engine 2 (the restarting HOST): opens with the SAME fixed port + a
    // sink — adoption must RE-BIND the listener live (the AI-46 closure).
    let shared = Arc::new(Mutex::new(Vec::new()));
    let engine2 = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            metering_listen_port: port,
            diagnostics: Some(make_sink(&shared)),
            ..EngineOptions::default()
        },
    )
    .expect("engine2 open (the restarting host)");
    let facade = engine2.blocking();
    let notes = sink_text(&shared);
    assert!(
        notes.contains(&format!(
            "re-bound the observed-metering listener on the fixed loopback port {port}"
        )),
        "the re-bind must be announced:\n{notes}"
    );
    assert!(
        !notes.contains("stranded observed listener"),
        "a live re-bind must NOT strand-warn:\n{notes}"
    );
    // The listener ANSWERS again — the adopted agent's traffic can flow.
    assert!(
        port_answer(port),
        "the re-bound listener must accept connects"
    );
    // Cleanup: stop the adopted agent (the host's drain would).
    facade
        .stop("observed", Some(Duration::from_secs(5)))
        .unwrap();
    wait_until_gone(pid, "the adopted agent must be gone after the stop");
}

#[test]
fn an_ephemeral_open_adoption_keeps_the_strand_diagnostic() {
    // The 11-3 posture is byte-unchanged when the restarting engine opens
    // with NO fixed port (the pre-17-2 default): the adoption still warns
    // the strand (no listener to re-bind), and no re-bind note appears.
    if is_linux_ci() {
        return;
    }
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let dump = state.path().join("ephemeral.dump");
    write_observed_manifest(
        manifest.path(),
        "observedkind",
        &[
            "--linger-ms",
            "600000",
            "--dump",
            &dump.display().to_string(),
        ],
    );
    let port = free_port();
    run_engine1_fixed_port(state.path(), manifest.path(), port);
    let pid = wait_for_agent_pid(&agent_log_path(state.path(), "observed"));
    // The NFR-1 crash-survival vehicle is Unix-only: on Windows the engine's
    // Job-Object kill-on-close takes the observed child the moment engine1's
    // process exits (the documented no-orphan guarantee), so the live-orphan
    // strand posture this test pins cannot exist there — the upstream 11-3
    // proof (adoption.rs's ai46 strand test) is Windows-gated on the same
    // survival harness. Pin the documented Windows outcome (no leaked child)
    // and stop; the ephemeral-open posture is exercised where an orphan can
    // survive. Runtime branch per the boundary gate's data-driven OS rule.
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        wait_until_gone(
            pid,
            "NFR-1: the crashed engine1 must not leak the observed child on Windows",
        );
        return;
    }
    let shared = Arc::new(Mutex::new(Vec::new()));
    // The DEFAULT open (port 0) — the pre-17-2 shape, byte-identical. Held
    // for the test's duration (the host-like shape: the engine stays open).
    let _engine2 = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            diagnostics: Some(make_sink(&shared)),
            ..EngineOptions::default()
        },
    )
    .expect("ephemeral open");
    let notes = sink_text(&shared);
    assert!(
        notes.contains("stranded observed listener"),
        "the 11-3 strand note must stay under an ephemeral open:\n{notes}"
    );
    assert!(
        !notes.contains("re-bound the observed-metering listener"),
        "no re-bind happens on an ephemeral open:\n{notes}"
    );
    // Cleanup: kill the stranded orphan (a stop would work too, but the
    // strand posture owes it nothing — the kill mirrors the operator's
    // out-of-band cleanup of an unreachable agent).
    kill_pid(pid);
    wait_until_gone(pid, "the stranded orphan must be cleaned up");
}

#[test]
fn serve_mode_children_inherit_only_their_declared_environment() {
    // The Host allowlist: the child's env is EXACTLY the declared mappings
    // PLUS the documented OS-baseline FLOOR (review finding 7 — Unix:
    // PATH/HOME, the minimal plumbing a child needs to start and resolve
    // programs; see docs/commands.md). Everything ELSE ambient — CI
    // variables, shell exports — still never passes.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let dump = state.path().join("allowlist.dump");
    write_observed_manifest(
        manifest.path(),
        "allowlistkind",
        &[
            "--linger-ms",
            "600000",
            "--dump",
            &dump.display().to_string(),
        ],
    );
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            child_env_allowlist: true,
            ..EngineOptions::default()
        },
    )
    .unwrap();
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "allowlist",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_config(
            "allowlist",
            "metering.upstream_base_url",
            "http://127.0.0.1:9/",
        )
        .unwrap();
    facade.start("allowlist").unwrap();
    // The DECLARED mapping arrived (the engine's injection survived the
    // clear — the value's PORT is the ephemeral pick here, so needle on the
    // prefix).
    let text = poll_dump_for(&dump, "env=OPENAI_BASE_URL=http://127.0.0.1:");
    // The FLOOR arrived: the child can resolve programs and find its home.
    // (Both are set in every dev/CI shell this unix leg runs in.)
    for floor in ["env=PATH=", "env=HOME="] {
        assert!(
            text.contains(floor),
            "the floor must keep {floor} for the child (review finding 7); full dump:\n{text}"
        );
    }
    // And NOTHING ELSE ambient did: no common CI/shell variables.
    for ambient in ["env=CI=", "env=CARGO=", "env=RUSTUP_HOME="] {
        assert!(
            !text.contains(ambient),
            "the allowlisted child must NOT see {ambient}; full dump:\n{text}"
        );
    }
    facade
        .stop("allowlist", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn plain_open_children_inherit_the_environment_unchanged() {
    // The DEFAULT open (allowlist off) is byte-identical to every pre-17-2
    // release: the child sees the full inherited environment PLUS the
    // declared mapping.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let dump = state.path().join("inherit.dump");
    write_observed_manifest(
        manifest.path(),
        "inheritkind",
        &[
            "--linger-ms",
            "600000",
            "--dump",
            &dump.display().to_string(),
        ],
    );
    let engine = Engine::open(Some(state.path().to_path_buf())).unwrap();
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "inherit",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_config(
            "inherit",
            "metering.upstream_base_url",
            "http://127.0.0.1:9/",
        )
        .unwrap();
    facade.start("inherit").unwrap();
    let text = poll_dump_for(&dump, "env=OPENAI_BASE_URL=http://127.0.0.1:");
    assert!(
        text.contains("env=PATH="),
        "the default (inherit) child must still see PATH:\n{text}"
    );
    facade
        .stop("inherit", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn a_pinned_port_already_held_refuses_the_start_naming_the_port() {
    // Review finding 12's supervisor-path pin: an engine-observed START
    // whose fixed port is ALREADY HELD by another process refuses with the
    // typed `ObservedMetering` error NAMING the port — never a silent hop
    // to an ephemeral substitute. (The raw `start_on_port` seam has its own
    // listener-level test; THIS one drives the full supervisor start path,
    // so a plausible ephemeral-fallback mutation ships red here.) The
    // refusal fires BEFORE any side effect — no spawn, no transition.
    let held = free_port();
    let _holder = std::net::TcpListener::bind(("127.0.0.1", held)).unwrap();
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_observed_manifest(manifest.path(), "collisionkind", &["--linger-ms", "600000"]);
    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            metering_listen_port: held,
            ..EngineOptions::default()
        },
    )
    .unwrap();
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "collision",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_config(
            "collision",
            "metering.upstream_base_url",
            "http://127.0.0.1:9/",
        )
        .unwrap();
    let err = facade.start("collision").unwrap_err();
    match &err {
        EngineError::ObservedMetering { name, detail } => {
            assert_eq!(name, "collision");
            assert!(
                detail.contains(&held.to_string()),
                "the bind refusal must NAME the held port {held}: {detail}"
            );
        }
        other => panic!("the collision must be the typed ObservedMetering error, got: {other:?}"),
    }
    // No silent hop: the held port still answers only for the HOLDER (the
    // engine never bound anything new there), and no child was spawned for
    // the refused start (the instance never left its pre-start state's
    // process-less posture — nothing to clean up).
    drop(_holder);
}

#[test]
fn both_backend_sources_carry_the_env_clear_guard() {
    // The Windows-leg compile pin, per the 11-5 pattern: BOTH backend spawn
    // implementations branch on `inherit_env` with `env_clear` (the
    // clear-then-apply contract) and keep the OS-baseline FLOOR (review
    // finding 7) between the clear and the apply; a pure source scan — no
    // OS-cfg, no Windows build needed in this suite.
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let floors: [(&str, &[&str]); 2] = [
        ("backends/unix/mod.rs", &["\"PATH\"", "\"HOME\""]),
        (
            "backends/windows/mod.rs",
            &[
                "\"SystemRoot\"",
                "\"SystemDrive\"",
                "\"ComSpec\"",
                "\"TEMP\"",
                "\"TMP\"",
                "\"PATH\"",
            ],
        ),
    ];
    for (backend_rel, floor_keys) in floors {
        let backend = src.join(backend_rel);
        let source = std::fs::read_to_string(&backend)
            .unwrap_or_else(|err| panic!("read {}: {err}", backend.display()));
        assert!(
            source.contains("if !spec.inherit_env {"),
            "{} must branch on inherit_env",
            backend.display()
        );
        assert!(
            source.contains("command.env_clear();"),
            "{} must clear the environment when inheritance is off",
            backend.display()
        );
        // The FLOOR (review finding 7): every documented OS-baseline key is
        // named in THIS backend's floor const — the minimal plumbing a
        // child keeps beyond its declared mappings. Pinned per source so a
        // backend silently dropping a floor key (e.g. a Windows cleanup
        // removing ComSpec) ships red here, on every OS.
        for key in floor_keys {
            assert!(
                source.contains(key),
                "{} must name its floor key {key} (review finding 7)",
                backend.display()
            );
        }
        // The floor loop sits between the clear and the declared-apply loop
        // (clear → floor → apply, never apply-then-clear which would drop
        // the declared mappings or lose the floor).
        let clear = source.find("command.env_clear();").expect("clear position");
        let floor = source
            .find("ENV_FLOOR.contains(&k)")
            .expect("floor position");
        let apply = source
            .find("for (key, value) in &spec.env {")
            .expect("apply position");
        assert!(
            clear < floor && floor < apply,
            "{} must order clear → floor → apply",
            backend.display()
        );
    }
}
