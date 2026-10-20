//! Integration tests for `hekma serve` (story 17-2) — the CLI-first surface,
//! end-to-end against the REAL binary (`CARGO_BIN_EXE_hekma`) and a REAL
//! `fake_agent`:
//!
//! * SIGTERM stops a running agent within the bounded window and the host
//!   exits 0 (the drain path — exit 0 even though it ran the ladder).
//! * A second `hekma serve` on the same state root is REFUSED with exit 2
//!   naming the holder PID + remediation (the frozen Usage class — no new
//!   exit-code number, DC-4).
//! * A repeated SIGTERM is idempotent: one drain, one per-instance outcome.
//! * Parse pins: `serve --help` (exit 0), a bad `--stop-window` (clap
//!   parse refusal, exit 2).
//!
//! POSIX shapes (`kill`, SIGTERM) are Unix-only: the Windows legs runtime-
//! skip via the data-driven OS id (NO `#[cfg]` — this file is outside the
//! backends/ allowlist), exactly the `lifecycle.rs` pattern.

mod helpers;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hekma_conformance::test_support::ManifestFixture;
use hekma_engine::AdapterRef;
use helpers::{run_hekma_agent, TestContext};

/// Whether the runtime OS is Windows (the runtime-skip arm).
fn is_windows() -> bool {
    hekma_engine::OsId::current() == hekma_engine::OsId::Windows
}

/// A manifest whose start exec is the fake_agent, lingering ~forever.
fn fake_agent_manifest(dir: &Path, name: &str) -> std::path::PathBuf {
    let m = dir.join(format!("{name}-adapter"));
    ManifestFixture::new("fake", &["--linger-ms", "600000"])
        .capability_on_os("interaction", current_os_key(), "guaranteed")
        .write(&m);
    m
}

fn current_os_key() -> &'static str {
    match hekma_engine::OsId::current() {
        hekma_engine::OsId::Windows => "windows",
        hekma_engine::OsId::Macos => "macos",
        // Linux and the "Other" bucket (e.g. the BSDs) share the linux key
        // in the conformance fixtures (its manifest schema has three legs).
        _ => "linux",
    }
}

/// The agent.log path inside an instance's Agent Home.
fn agent_log_path(state_dir: &Path, name: &str) -> std::path::PathBuf {
    state_dir
        .join("agents")
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

/// Whether a pid is still alive (`kill -0`; tasklist on Windows).
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
        _ => Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
    }
}

/// Spawn `hekma serve` in the BACKGROUND over `state_dir` with piped
/// stdio, and wait for readiness (the state DB's lock file exists AND the
/// process is alive). The readiness marker — the engine is open and the
/// lock held when `host.lock` appears. Extra args ride along verbatim
/// (review finding 15: the `--metering-listen` / `--state-dir` flag legs
/// pin the CLI wiring through the REAL binary).
fn spawn_serve_with(
    state_dir: &Path,
    extra_args: &[&str],
) -> (std::process::Child, std::thread::JoinHandle<String>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_hekma"))
        .args(["serve"])
        .args(extra_args)
        .env("KTESIO_NO_UPDATE_CHECK", "1")
        .env("KTESIO_STATE_DIR", state_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hekma serve");
    // Drain stderr on a thread (the pipe must not fill while we wait), and
    // use it as the readiness signal.
    let mut stderr = child.stderr.take().expect("piped stderr");
    let drainer = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    // The drainer owns the pipe; poll the CHILD's liveness + a shared
    // snapshot instead: simplest correct readiness check is to wait on a
    // sentinel — peek via /proc-free approach: wait until the state DB's
    // lock file exists AND the process is alive, with the notice asserted
    // from the drained buffer at the end. Readiness = host.lock exists.
    let lock = state_dir.join(hekma_engine::paths::HOST_LOCK_FILE);
    loop {
        if lock.exists() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "serve never became ready (no host.lock)"
        );
        if child.try_wait().expect("try_wait").is_some() {
            panic!("serve exited before becoming ready");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    (child, drainer)
}

/// The default spawner: env-var state dir, no extra flags.
fn spawn_serve(state_dir: &Path) -> (std::process::Child, std::thread::JoinHandle<String>) {
    spawn_serve_with(state_dir, &[])
}

/// Send SIGTERM to the serve process and wait (bounded) for exit.
fn signal_and_wait(child: &mut std::process::Child, what: &str) -> std::process::ExitStatus {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill shell-out");
    assert!(status.success(), "{what}: the kill shell-out failed");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: serve did not exit after SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn serve_stops_a_running_agent_on_sigterm_and_exits_zero() {
    if is_windows() {
        return; // POSIX `kill`-based signals; on Windows the same host surface
                // is exercised by the signal-injected factory tests (the
                // lib-level `serve_with_signal` runs) — the real-Ctrl+C e2e
                // is deferred (see deferred-work.md, "real Windows Ctrl+C
                // e2e needs a console-event harness").
    }
    let ctx = TestContext::new();
    let state_dir = ctx.project_dir.join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let manifest = fake_agent_manifest(&ctx.project_dir, "hosted");
    run_hekma_agent(
        [
            "agent",
            "register",
            "hosted",
            "--manifest",
            manifest.to_str().unwrap(),
        ]
        .as_slice(),
        &ctx.project_dir,
        &state_dir,
    );
    // DETACHED start (story 12-1): the agent survives the start command's
    // engine exit — exactly the between-commands state a HOST serves. The
    // host's open then ADOPTS it (the story 15-2 resume + adoption notes).
    let started = run_hekma_agent(
        ["agent", "start", "hosted", "--detach"].as_slice(),
        &ctx.project_dir,
        &state_dir,
    );
    assert!(started.success, "agent start failed: {}", started.stderr);
    let pid = wait_for_agent_pid(&agent_log_path(&state_dir, "hosted"));

    let (mut serve, drainer) = spawn_serve(&state_dir);
    let status = signal_and_wait(&mut serve, "with a running agent");
    assert_eq!(status.code(), Some(0), "the drain path exits 0: {status}");
    let deadline = Instant::now() + Duration::from_secs(8);
    while pid_alive(pid) {
        assert!(Instant::now() < deadline, "the agent survived the drain");
        std::thread::sleep(Duration::from_millis(30));
    }
    let stderr = drainer.join().unwrap();
    for expected in [
        "host started",
        "SIGTERM received",
        "stopping hosted",
        "stopped",
        "shutdown complete",
    ] {
        assert!(
            stderr.contains(expected),
            "the serve stream must contain {expected:?}; full stream:\n{stderr}"
        );
    }
}

#[test]
fn a_second_serve_is_refused_with_exit_two_naming_the_holder() {
    if is_windows() {
        return;
    }
    let ctx = TestContext::new();
    let state_dir = ctx.project_dir.join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let (mut first, first_drainer) = spawn_serve(&state_dir);
    // The second serve BLOCKS-and-refuses: exit 2, holder named.
    let refused = run_hekma_agent(["serve"].as_slice(), &ctx.project_dir, &state_dir);
    assert_eq!(
        refused.code,
        Some(2),
        "the lock refusal is Usage-class (exit 2): {}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains("already served by another host"),
        "the refusal names the condition: {}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains(&format!("pid {}", first.id())),
        "the refusal names the LIVE holder pid {}: {}",
        first.id(),
        refused.stderr
    );
    assert!(
        refused.stderr.contains("stop that host first")
            && refused.stderr.contains("serve a different state directory"),
        "the refusal carries the remediation: {}",
        refused.stderr
    );
    // Review finding 10's pin: NO file-removal advice — a dead holder
    // releases the OS lock by itself, and deletion advice invites deleting a
    // LIVE holder's lock (a third serve would then mint a fresh host.lock
    // inode and two hosts would serve one root).
    assert!(
        !refused.stderr.contains("remove the stale lock file"),
        "no file-removal advice: {}",
        refused.stderr
    );
    // The first host is unaffected; a clean SIGTERM shutdown still exits 0.
    let status = signal_and_wait(&mut first, "after the refused second");
    assert_eq!(status.code(), Some(0));
    let stderr = first_drainer.join().unwrap();
    assert!(stderr.contains("shutdown complete"), "stream:\n{stderr}");
}

#[test]
fn a_repeated_sigterm_is_idempotent_one_drain() {
    if is_windows() {
        return;
    }
    let ctx = TestContext::new();
    let state_dir = ctx.project_dir.join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let manifest = fake_agent_manifest(&ctx.project_dir, "twice");
    run_hekma_agent(
        [
            "agent",
            "register",
            "twice",
            "--manifest",
            manifest.to_str().unwrap(),
        ]
        .as_slice(),
        &ctx.project_dir,
        &state_dir,
    );
    let started = run_hekma_agent(
        ["agent", "start", "twice", "--detach"].as_slice(),
        &ctx.project_dir,
        &state_dir,
    );
    assert!(started.success, "agent start failed: {}", started.stderr);
    let (mut serve, drainer) = spawn_serve(&state_dir);
    // TWO signals, back to back: the drain must run exactly ONCE.
    for _ in 0..2 {
        let ok = Command::new("kill")
            .args(["-TERM", &serve.id().to_string()])
            .status()
            .expect("kill shell-out");
        assert!(ok.success());
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = serve.try_wait().expect("try_wait") {
            assert_eq!(status.code(), Some(0), "the drain path exits 0");
            break;
        }
        assert!(Instant::now() < deadline, "serve did not exit");
        std::thread::sleep(Duration::from_millis(30));
    }
    let stderr = drainer.join().unwrap();
    assert_eq!(
        stderr.matches("shutdown complete").count(),
        1,
        "exactly one shutdown, however many signals:\n{stderr}"
    );
    assert_eq!(
        stderr.matches("SIGTERM received").count(),
        1,
        "the wait resolves once; the second signal cannot re-drain:\n{stderr}"
    );
    assert_eq!(
        stderr.matches("stopping twice").count(),
        1,
        "the agent is stopped exactly once:\n{stderr}"
    );
}

#[test]
fn serve_help_and_bad_stop_window_parse_pins() {
    // `--help` exits 0 and shows the flags (clap's own surface).
    let help = run_hekma_agent(["serve", "--help"].as_slice(), &ctx_dir(), &ctx_dir());
    assert!(help.success, "serve --help must succeed: {}", help.stderr);
    assert!(
        help.stdout.contains("--stop-window")
            && help.stdout.contains("--metering-listen")
            && help.stdout.contains("--state-dir"),
        "the help names the flags:\n{}",
        help.stdout
    );
    // `--stop-window 0` is outside the 1..=86400 range: clap parse refusal,
    // exit 2. The UPPER bound is pinned too (review finding 9: a multi-day
    // window is a rejected invocation on every surface).
    let bad = run_hekma_agent(
        ["serve", "--stop-window", "0"].as_slice(),
        &ctx_dir(),
        &ctx_dir(),
    );
    assert_eq!(
        bad.code,
        Some(2),
        "a sub-1 window is a parse refusal: {}",
        bad.stderr
    );
    assert!(
        bad.stderr.contains("--stop-window"),
        "clap names the offending flag: {}",
        bad.stderr
    );
    let bad_high = run_hekma_agent(
        ["serve", "--stop-window", "86401"].as_slice(),
        &ctx_dir(),
        &ctx_dir(),
    );
    assert_eq!(
        bad_high.code,
        Some(2),
        "a window over 86400 is a parse refusal: {}",
        bad_high.stderr
    );
}

/// A throwaway cwd for the parse pins (they open nothing).
fn ctx_dir() -> std::path::PathBuf {
    std::env::temp_dir()
}

#[test]
fn serve_state_dir_flag_serves_that_root() {
    // Review finding 15's CLI-level pin: `--state-dir <dir>` (the FLAG, not
    // the env var) serves THAT root — the lock and the state store land in
    // the flagged directory, and the notice names it.
    if is_windows() {
        return; // POSIX `kill`-based shutdown: the runner's `kill` cannot
                // signal the native serve child ("No such process"). On
                // Windows the state-root routing is exercised lib-level
                // (`ServeOptions::state_base` through `serve_with_signal`)
                // and the graceful-shutdown e2e shares the deferred
                // console-event harness (see deferred-work.md, "real
                // Windows Ctrl+C e2e needs a console-event harness").
    }
    let ctx = TestContext::new();
    let flagged = ctx.project_dir.join("flagged-root");
    std::fs::create_dir_all(&flagged).unwrap();
    let (mut serve, drainer) =
        spawn_serve_with(&flagged, &["--state-dir", flagged.to_str().unwrap()]);
    let status = signal_and_wait(&mut serve, "the --state-dir leg");
    assert_eq!(status.code(), Some(0), "a clean shutdown exits 0");
    let stderr = drainer.join().unwrap();
    assert!(
        stderr.contains("host started") && stderr.contains(&flagged.to_string_lossy().to_string()),
        "the notice names the FLAGGED root:\n{stderr}"
    );
    assert!(
        flagged.join(hekma_engine::paths::HOST_LOCK_FILE).exists(),
        "the lock landed in the flagged root"
    );
    assert!(flagged.join("state.db").exists(), "the engine opened there");
}

#[test]
fn serve_helper_subprocess() {
    // The re-exec entry (the serve_host.rs pattern): when the env var is
    // unset this is a trivial pass (a normal test in the parent binary).
    // When set: open the engine over the state dir, register + start the
    // engine-observed instance, then EXIT WITHOUT DROPPING — the crashed-host
    // shape whose observed child survives re-parented to init, pointing at
    // this engine's now-dead (ephemeral) listener. (`--detach` cannot build
    // this state: the story-12-1 refusal correctly forbids detaching an
    // observed instance, so the surviving-child shape needs the abrupt
    // exit.)
    let Ok(state) = std::env::var("KTESIO_SERVE_HELPER_STATE") else {
        return; // normal in-process invocation: nothing to do.
    };
    let manifest = std::env::var("KTESIO_SERVE_HELPER_MANIFEST").expect("helper manifest");
    let engine =
        hekma_engine::Engine::open(Some(std::path::PathBuf::from(state))).expect("helper open");
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "observed",
            &AdapterRef::Manifest(std::path::PathBuf::from(manifest)),
        )
        .expect("helper register");
    facade
        .set_config(
            "observed",
            "metering.upstream_base_url",
            "http://127.0.0.1:9/",
        )
        .expect("helper config");
    facade.start("observed").expect("helper start");
    // The crashed host: no drop, no drain, no kill-on-drop — the child
    // survives.
    std::process::exit(0);
}

#[test]
fn serve_metering_listen_flag_pins_the_observed_listener() {
    // Review finding 15's CLI-level pin: `--metering-listen <port>` really
    // pins the observed-metering listener — an ADOPTED engine-observed
    // instance is re-bound on THAT port (the note names it) and the port
    // ANSWERS connects while the host serves.
    if is_windows() {
        return; // POSIX `kill`-based readiness/signals on the sibling legs.
    }
    let ctx = TestContext::new();
    let state_dir = ctx.project_dir.join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    // An engine-observed manifest (the serve_host.rs shape): the mapping the
    // engine's injection targets, the fake_agent vehicle.
    let bin = hekma_conformance::fake_agent_bin();
    let manifest = ctx.project_dir.join("observed-adapter");
    std::fs::create_dir_all(&manifest).unwrap();
    std::fs::write(
        manifest.join("adapter.toml"),
        format!(
            r#"
contract_version = "1.0.0"

[adapter]
kind = "observedhosted"

[lifecycle.start]
exec = {exec:?}
args = ["--linger-ms", "600000"]

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
        ),
    )
    .unwrap();
    // The crashed-host setup: this test binary re-execs its helper entry,
    // which starts the observed instance and exits without dropping.
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["serve_helper_subprocess", "--exact"])
        .env("KTESIO_SERVE_HELPER_STATE", &state_dir)
        .env("KTESIO_SERVE_HELPER_MANIFEST", &manifest)
        .env("LLVM_PROFILE_FILE", hekma_conformance::helper_profile_env())
        .status()
        .expect("run serve-helper subprocess");
    assert!(status.success(), "serve-helper subprocess failed: {status}");
    // The port pick: the standard bind-0-then-drop probe.
    let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let (mut serve, drainer) =
        spawn_serve_with(&state_dir, &["--metering-listen", &port.to_string()]);
    // The host's ADOPTION of the orphaned observed instance re-binds the
    // listener on the pinned port — wait until the listener ANSWERS (the
    // observable; the stderr pipe is owned by the drainer thread, and the
    // note is asserted from the drained buffer after shutdown).
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the pinned port never came live (no re-bind)"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    let status = signal_and_wait(&mut serve, "the --metering-listen leg");
    assert_eq!(status.code(), Some(0));
    let stderr = drainer.join().unwrap();
    assert!(
        stderr.contains(&format!(
            "re-bound the observed-metering listener on the fixed loopback port {port}"
        )),
        "the adoption re-bind names the pinned port:\n{stderr}"
    );
}
