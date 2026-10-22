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
//! skip via the data-driven OS id (the `lifecycle.rs` pattern). ONE
//! exception since 17-3 (review item 24b): the Control-API e2e leg writes a
//! 0600 `secrets.toml` — the OS permission boundary IS the API there — so
//! this file carries a single `#[cfg(unix)]` chmod, disclosed through the
//! integration-test allowlist (AI-35) like the host suite's.

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
            && help.stdout.contains("--state-dir")
            && help.stdout.contains("--log-capture"),
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
    // Story 17-4 (the parse-pin lesson, real-binary leg): an invalid
    // `--log-capture` value is a clap PARSE refusal — exit 2, the flag
    // named — never a silent fall-back to a different mode.
    let bad_mode = run_hekma_agent(
        ["serve", "--log-capture", "sometimes"].as_slice(),
        &ctx_dir(),
        &ctx_dir(),
    );
    assert_eq!(
        bad_mode.code,
        Some(2),
        "an invalid log-capture mode is a parse refusal: {}",
        bad_mode.stderr
    );
    assert!(
        bad_mode.stderr.contains("--log-capture"),
        "clap names the offending flag: {}",
        bad_mode.stderr
    );
}

/// A throwaway cwd for the parse pins (they open nothing).
fn ctx_dir() -> std::path::PathBuf {
    std::env::temp_dir()
}

#[test]
fn serve_option_refusals_exit_by_the_frozen_classes() {
    // The CLI-first error classes on the serve surface: a half-configured
    // Control API (`--listen` without its bearer secret) is a rejected
    // INVOCATION (Usage class, exit 2, the remediation named), and an
    // unusable state root (a FILE occupying it) is an environment/system
    // failure (Startup class, exit 1) — never a panic, never a wrong class.
    let no_secret = run_hekma_agent(
        ["serve", "--listen", "127.0.0.1:18499"].as_slice(),
        &ctx_dir(),
        &ctx_dir(),
    );
    assert_eq!(
        no_secret.code,
        Some(2),
        "a half-configured API is a Usage-class refusal"
    );
    assert!(
        no_secret.stderr.contains("--bearer-secret"),
        "the remediation is named in stderr"
    );

    let blocker = ctx_dir().join("hekma-serve-state-blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let startup = run_hekma_agent(["serve"].as_slice(), &ctx_dir(), &blocker);
    assert_eq!(
        startup.code,
        Some(1),
        "an unusable state root is a Startup-class failure: {}",
        startup.stderr
    );
    let _ = std::fs::remove_file(&blocker);
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

// ---------------------------------------------------------------------------
// Story 17-3 (review item 24b): the Control-API flag pair over the REAL
// binary. The clap-parse half of this pin lives in `crates/hekma/src/lib.rs`
// (`test_serve_control_api_flag_pair_lands_in_the_right_fields`); THIS leg
// proves the WIRING: serve with the pair, the bearer resolved from the
// state dir's secrets file, `/v0/version` answered 200 WITH the bearer and
// 401 without — then the bounded SIGTERM shutdown, exit 0.
// ---------------------------------------------------------------------------

/// The bearer secret NAME the real-binary leg resolves (the secrets-file
/// leg; the process env is left untouched).
const CONTROL_API_BEARER_NAME: &str = "serve_cli_control_bearer";
/// The bearer token VALUE that name resolves to.
const CONTROL_API_BEARER_VALUE: &str = "serve-cli-bearer-token-17-3";

/// A minimal raw-socket `GET /v0/version`: returns the HTTP status code, or
/// `None` when the connection cannot even form (the API is not up yet). The
/// `hekma` crate carries no HTTP dev-dependency — the check is two raw
/// sockets wide.
fn raw_version_status(addr: &str, with_bearer: bool) -> Option<u16> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(addr).ok()?;
    let auth = if with_bearer {
        format!("Authorization: Bearer {CONTROL_API_BEARER_VALUE}\r\n")
    } else {
        String::new()
    };
    stream
        .write_all(
            format!("GET /v0/version HTTP/1.1\r\nHost: {addr}\r\n{auth}Connection: close\r\n\r\n")
                .as_bytes(),
        )
        .ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    response
        .strip_prefix("HTTP/1.1 ")?
        .split(' ')
        .next()?
        .parse()
        .ok()
}

/// A minimal authenticated HTTP/1.1 request over a fresh TCP connection,
/// returning `(status, full response text)`; `None` = the API never
/// answered. The `body` rides with `Content-Length` (an absent body is the
/// empty string).
fn raw_request(addr: &str, method: &str, path: &str, body: Option<&str>) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(addr).ok()?;
    let body = body.unwrap_or("");
    stream
        .write_all(
            format!(
                "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer \
                 {CONTROL_API_BEARER_VALUE}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let status = response
        .strip_prefix("HTTP/1.1 ")?
        .split(' ')
        .next()?
        .parse()
        .ok()?;
    Some((status, response))
}

/// The response body of a raw response text (past the header blank line).
fn response_body(response: &str) -> &str {
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("")
}

#[test]
fn serve_log_capture_metadata_leaves_no_agent_content_at_rest_and_exports_events() {
    if is_windows() {
        return; // POSIX `kill`-based SIGTERM; see the sibling test's note.
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

    // The bearer secret rides the SECRETS FILE leg (same chmod discipline
    // as the sibling control-api test — the AI-35-disclosed #[cfg(unix)]).
    let secrets = state_dir.join(hekma_engine::paths::SECRETS_FILE);
    std::fs::write(
        &secrets,
        format!("{CONTROL_API_BEARER_NAME} = \"{CONTROL_API_BEARER_VALUE}\"\n"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let port = {
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        probe.local_addr().unwrap().port()
    };
    let (mut child, drainer) = spawn_serve_with(
        &state_dir,
        &[
            // Story 17-4: the HOST spawns every agent in METADATA mode —
            // this e2e proves the at-rest shape through the REAL binary.
            "--log-capture",
            "metadata",
            "--listen",
            &format!("127.0.0.1:{port}"),
            "--bearer-secret",
            CONTROL_API_BEARER_NAME,
        ],
    );
    let addr = format!("127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match raw_request(&addr, "GET", "/v0/version", None) {
            Some((200, _)) => break,
            Some((status, _)) => panic!("the version probe answered {status}"),
            None => assert!(Instant::now() < deadline, "the control api never answered"),
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // START through the HOST — the host's engine spawns with the metadata
    // posture (the CLI-engine detached-start path would be Full).
    let (status, _) = raw_request(&addr, "POST", "/v0/instances/hosted/start", None)
        .expect("the start route answered");
    assert_eq!(status, 200, "the host-mode start must succeed");
    let agent_log = agent_log_path(&state_dir, "hosted");
    let pid = wait_for_agent_pid(&agent_log);

    // WHILE RUNNING: the raw capture exists (the transient window is the
    // metadata contract), and `output.log` is METADATA-shaped — records,
    // never the agent's content.
    let output_log = state_dir
        .join("agents")
        .join("hosted")
        .join("logs")
        .join("output.log");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(&output_log) {
            if text.contains("\"record\":\"metadata\"") {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "metadata records never appeared in output.log"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let output_text = std::fs::read_to_string(&output_log).unwrap();
    assert!(
        !output_text.contains("ready pid="),
        "no raw agent content may land in a metadata-mode output.log:\n{output_text}"
    );
    assert!(
        output_text.contains("\"byte_len\"") && output_text.contains("\"sha256\""),
        "the metadata records carry the size/hash facts:\n{output_text}"
    );

    // STOP through the host: the raw files are UNLINKED (metadata-only at
    // rest), the metadata-shaped output.log remains.
    let (status, _) = raw_request(&addr, "POST", "/v0/instances/hosted/stop", Some("{}"))
        .expect("the stop route answered");
    assert_eq!(status, 200, "the host-mode stop must succeed");
    let deadline = Instant::now() + Duration::from_secs(10);
    while pid_alive(pid) {
        assert!(Instant::now() < deadline, "the agent survived the stop");
        std::thread::sleep(Duration::from_millis(30));
    }
    let stderr_log = state_dir
        .join("agents")
        .join("hosted")
        .join("logs")
        .join("agent-stderr.log");
    assert!(
        !agent_log.exists(),
        "metadata mode unlinks agent.log at stop"
    );
    assert!(
        !stderr_log.exists(),
        "metadata mode unlinks agent-stderr.log at stop"
    );
    assert!(
        output_log.exists(),
        "the metadata-shaped output.log remains at rest"
    );

    // The host's committed transitions (start/stop) are in the GLOBAL
    // outbox: the export pull serves them verbatim, and the ack cursor is
    // monotonic + idempotent. A from-zero pull is EXPLICIT (the default
    // resume-from-ack is the common case; this leg pins the explicit form).
    let (status, response) =
        raw_request(&addr, "GET", "/v0/export?cursor=%7B%22seq%22%3A0%7D", None)
            .expect("the export route answered");
    assert_eq!(status, 200, "the export pull succeeded: {response}");
    let body = response_body(&response);
    assert!(
        body.contains("\"events\"") && body.contains("\"cursor\""),
        "the ExportBatch wire shape is served verbatim:\n{body}"
    );
    // The highest exported seq = the ack target: everything the pull showed.
    let max_seq: u64 = body
        .split("\"seq\":")
        .skip(1)
        .filter_map(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|digits| digits.parse::<u64>().ok())
        })
        .max()
        .expect("the batch carries seqs");
    let (status, response) = raw_request(
        &addr,
        "POST",
        "/v0/export/ack",
        Some(&format!("{{\"seq\":{max_seq}}}")),
    )
    .expect("the ack route answered");
    assert_eq!(status, 200, "the ack succeeded: {response}");
    assert!(
        response_body(&response).contains(&format!("\"seq\":{max_seq}")),
        "the ack returns the cursor AS PERSISTED: {response}"
    );
    // Idempotent re-ack: same persisted cursor, still 200.
    let (status, response) = raw_request(
        &addr,
        "POST",
        "/v0/export/ack",
        Some(&format!("{{\"seq\":{max_seq}}}")),
    )
    .expect("the re-ack answered");
    assert_eq!(status, 200, "a re-ack is an idempotent 200: {response}");
    // An ack PAST the committed tail is the typed 409, never a clamp.
    let (status, response) = raw_request(&addr, "POST", "/v0/export/ack", Some("{\"seq\":999999}"))
        .expect("the past-tail ack answered");
    assert_eq!(status, 409, "a past-tail ack is 409: {response}");
    assert!(
        response.contains("cursor_invalid"),
        "the 409 names the cursor_invalid kind: {response}"
    );

    // The bounded SIGTERM shutdown — exit 0 — and the notice record.
    let status = signal_and_wait(&mut child, "the metadata e2e leg");
    assert_eq!(status.code(), Some(0), "serve must exit 0 after SIGTERM");
    let stderr = drainer.join().unwrap();
    assert!(
        stderr.contains("shutdown complete"),
        "the shutdown notice is on the record:\n{stderr}"
    );
}

#[test]
fn serve_control_api_flag_pair_resolves_the_bearer_and_answers_over_the_wire() {
    if is_windows() {
        return; // POSIX `kill`-based SIGTERM; see the sibling test's note.
    }
    let ctx = TestContext::new();
    let state_dir = ctx.project_dir.join("state");
    std::fs::create_dir_all(&state_dir).unwrap();

    // The bearer secret rides the SECRETS FILE leg. The Unix chmod is
    // REQUIRED — the resolver refuses a group/other-accessible secrets file
    // (the AI-35-disclosed `#[cfg(unix)]`; this file's allowlist entry was
    // added for exactly this write). On Windows the fresh-file ACL check
    // accepts a user-owned file.
    let secrets = state_dir.join(hekma_engine::paths::SECRETS_FILE);
    std::fs::write(
        &secrets,
        format!("{CONTROL_API_BEARER_NAME} = \"{CONTROL_API_BEARER_VALUE}\"\n"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    // A loopback port that is free RIGHT NOW (bind-then-drop probe).
    let port = {
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        probe.local_addr().unwrap().port()
    };
    let (mut child, drainer) = spawn_serve_with(
        &state_dir,
        &[
            "--listen",
            &format!("127.0.0.1:{port}"),
            "--bearer-secret",
            CONTROL_API_BEARER_NAME,
        ],
    );

    // WITH the bearer: 200 (the surface self-describes). WITHOUT: 401.
    let addr = format!("127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = raw_version_status(&addr, true) {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the control api never answered on {addr}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(
        status, 200,
        "the resolved bearer authenticates over the wire"
    );
    let status = raw_version_status(&addr, false)
        .expect("the API that answered WITH a bearer must answer without one too");
    assert_eq!(status, 401, "an anonymous request is refused");

    // The bounded SIGTERM shutdown — exit 0 — and the notice record.
    let status = signal_and_wait(&mut child, "the control-api e2e leg");
    assert_eq!(status.code(), Some(0), "serve must exit 0 after SIGTERM");
    let stderr = drainer.join().unwrap();
    assert!(
        stderr.contains("control api listening on"),
        "the listening notice is on the record:\n{stderr}"
    );
    assert!(
        stderr.contains("control api stopped"),
        "the down notice is on the record:\n{stderr}"
    );
    assert!(
        !stderr.contains("NON-LOOPBACK"),
        "a loopback bind never warns:\n{stderr}"
    );
}
