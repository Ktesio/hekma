//! Integration tests for the host's serve lifecycle (story 17-2): the full
//! [`serve_with_signal`] cycle on a hermetic state root, the lock refusal
//! (second host), and the no-instance drain — all driven through the PUBLIC
//! library surface with a caller-supplied sink, no OS signal needed (the
//! signal seam is the factory; the real OS-signal path is proven by the
//! `hekma serve` CLI end-to-end test and the in-crate signals test).

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hekma_host::{serve_with_signal, ServeError, ServeOptions};

/// A sink that records every notice into a shared buffer the test can
/// assert on (the same shape the engine's own diagnostic-sink tests use).
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

/// The immediately-resolving signal factory — the "signal already arrived"
/// host run, with an honest name for the notice. The factory receives the
/// host's sink (the real source notes swallowed repeats through it); this
/// stub ignores it.
fn immediate_signal(
) -> impl FnOnce(hekma_engine::DiagnosticSink) -> std::future::Ready<Result<&'static str, String>> {
    |_sink| std::future::ready(Ok("SIGTEST"))
}

#[test]
fn a_full_host_cycle_completes_and_surfaces_every_notice() {
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, sink) = SharedSink::new();
    let options = ServeOptions {
        state_base: Some(state.path().to_path_buf()),
        diagnostics: Some(sink),
        ..ServeOptions::default()
    };
    let result = serve_with_signal(options, immediate_signal());
    assert!(
        result.is_ok(),
        "a clean no-agent serve must succeed: {result:?}"
    );
    let text = sink_handle.text();
    for expected in [
        "host started",
        "state root",
        "SIGTEST received",
        "no running agents; nothing to stop",
        "shutdown complete",
    ] {
        assert!(
            text.contains(expected),
            "the notice stream must contain {expected:?}; full stream:\n{text}"
        );
    }
    // The `[hekma] ` prefix discipline (the engine's diagnostic shape).
    assert!(
        text.contains("[hekma] "),
        "notices carry the prefix:\n{text}"
    );
    // The engine opened (its state store exists) — the host really served.
    assert!(state.path().join("state.db").exists(), "the engine opened");
}

#[test]
fn a_second_host_is_refused_naming_the_holder_without_opening_an_engine() {
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    // The FIRST host holds the lock (the same file the serve path would).
    let lock_path = base.join(hekma_engine::paths::HOST_LOCK_FILE);
    let _first = hekma_host::HostLock::acquire(&lock_path).expect("first host lock");
    let (sink_handle, sink) = SharedSink::new();
    let options = ServeOptions {
        state_base: Some(base.clone()),
        diagnostics: Some(sink),
        ..ServeOptions::default()
    };
    let err =
        serve_with_signal(options, immediate_signal()).expect_err("a second host must be refused");
    match &err {
        ServeError::StateDirLocked { lock_path, holder } => {
            // "Naming the holder" is platform-conditional in its EVIDENCE,
            // not its refusal: on Windows the record is unreadable while
            // the lock is held (per-handle `LockFileEx` — the lock module's
            // documented case), so the refusal honestly carries
            // `holder: None` there; the LOCK itself was proven by
            // `WouldBlock`. Unix advisory locks never block reads, so the
            // record names this test process's pid. Runtime branch, not
            // `#[cfg]` — the boundary gate's data-driven OS rule.
            let expected_holder = if std::env::consts::OS == "windows" {
                None
            } else {
                Some(std::process::id())
            };
            assert_eq!(
                holder, &expected_holder,
                "the refusal names the live holder's pid where the platform can read the record"
            );
            assert_eq!(
                lock_path,
                &base.join(hekma_engine::paths::HOST_LOCK_FILE),
                "the refusal names the lock path"
            );
        }
        other => panic!("expected StateDirLocked, got {other:?}"),
    }
    // AI-18: with a caller sink, the refusal is ALSO noted through it.
    let text = sink_handle.text();
    assert!(
        text.contains("already served by another host") && text.contains("pid"),
        "the refusal is surfaced through the sink:\n{text}"
    );
    // The engine NEVER opened: no state store exists (the lock runs BEFORE
    // Engine::open — a refused host touches nothing).
    assert!(
        !base.join("state.db").exists(),
        "a refused host must not open an engine"
    );
}

#[test]
fn a_refusal_without_a_sink_surfaces_through_the_error_alone() {
    // No diagnostics in the options: the returned error is the single
    // surfacing (the caller prints it) — the serve call must still refuse
    // with the full holder-naming text and not double-emit anywhere.
    let state = tempfile::tempdir().unwrap();
    let base = state.path().to_path_buf();
    let _first =
        hekma_host::HostLock::acquire(&base.join(hekma_engine::paths::HOST_LOCK_FILE)).unwrap();
    let options = ServeOptions {
        state_base: Some(base),
        ..ServeOptions::default()
    };
    let err = serve_with_signal(options, immediate_signal()).expect_err("refused");
    let text = err.to_string();
    // On Windows the holder record is unreadable while the lock is held
    // (per-handle `LockFileEx` — the lock module's documented case), so the
    // error alone carries the honest unreadable-record wording instead of
    // the pid. Runtime branch, not `#[cfg]` — the boundary gate's
    // data-driven OS rule.
    let holder_named = if std::env::consts::OS == "windows" {
        text.contains("its pid record could not be read")
    } else {
        text.contains(&std::process::id().to_string())
    };
    assert!(
        text.contains("already served by another host") && holder_named,
        "the error alone carries the holder + remediation: {text}"
    );
    // Review finding 10's pin: the remediation names the two REAL options —
    // stop that host, or serve a different state directory — and NEVER
    // advises deleting the lock file (a dead holder releases the OS lock by
    // itself; file-removal advice invites deleting a LIVE holder's lock,
    // which would let a third serve create a fresh lock inode and put TWO
    // hosts on one state root).
    assert!(
        text.contains("stop that host first") && text.contains("serve a different state directory"),
        "remediation present: {text}"
    );
    assert!(
        !text.contains("stale lock file") && !text.contains("remove"),
        "NO file-removal advice: {text}"
    );
}

#[test]
fn the_no_instance_drain_completes_with_no_outcomes() {
    // An engine with no instances at all: the drain plans nothing, says so
    // once, and returns an empty outcome list (the honest no-op).
    let state = tempfile::tempdir().unwrap();
    let engine =
        std::sync::Arc::new(hekma_engine::Engine::open(Some(state.path().to_path_buf())).unwrap());
    let (sink_handle, sink) = SharedSink::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let outcomes = rt.block_on(hekma_host::drain_and_stop(
        &engine,
        std::time::Duration::from_secs(1),
        &sink,
    ));
    assert!(
        outcomes.is_empty(),
        "no instances → no outcomes: {outcomes:?}"
    );
    let text = sink_handle.text();
    assert!(
        text.contains("no running agents; nothing to stop"),
        "the no-op is said, not silent:\n{text}"
    );
}

#[test]
fn the_options_debug_rendering_names_every_field_and_hides_the_sink() {
    // The manual `Debug` impl must name every option field (an operator
    // dumping their serve config reads real values, not `..`) while NEVER
    // printing the boxed writer sink (no-leak discipline — the sink is not
    // `Debug` and must stay out of the render).
    let (sink_handle, sink) = SharedSink::new();
    let with_sink = format!(
        "{:?}",
        ServeOptions {
            state_base: Some(PathBuf::from("/tmp/hekma-state")),
            stop_window: Some(9),
            metering_listen_port: 4321,
            control_listen: Some(
                "127.0.0.1:8400"
                    .parse::<std::net::SocketAddr>()
                    .expect("static test address parses")
            ),
            bearer_secret: Some("render_check_bearer".to_string()),
            log_capture: hekma_engine::LogCaptureMode::Metadata,
            diagnostics: Some(sink),
        }
    );
    assert!(
        with_sink.contains("control_listen") && with_sink.contains("127.0.0.1:8400"),
        "control_listen is rendered with its value: {with_sink}"
    );
    assert!(
        with_sink.contains("bearer_secret") && with_sink.contains("render_check_bearer"),
        "bearer_secret is rendered with its configured name: {with_sink}"
    );
    assert!(
        with_sink.contains("log_capture") && with_sink.contains("Metadata"),
        "log_capture is rendered with its configured mode: {with_sink}"
    );
    assert!(
        with_sink.contains("state_base") && with_sink.contains("/tmp/hekma-state"),
        "state_base is rendered with its value: {with_sink}"
    );
    assert!(
        with_sink.contains("stop_window") && with_sink.contains("9"),
        "stop_window is rendered with its value: {with_sink}"
    );
    assert!(
        with_sink.contains("metering_listen_port") && with_sink.contains("4321"),
        "metering_listen_port is rendered with its value: {with_sink}"
    );
    assert!(
        with_sink.contains("diagnostics") && with_sink.contains("<sink>"),
        "the sink renders as the <sink> placeholder, never its innards: {with_sink}"
    );
    assert!(
        !with_sink.contains("SharedSink"),
        "no sink internals leak into the debug render: {with_sink}"
    );
    let _ = sink_handle; // the sink is only carried, never written here
    let without_sink = format!("{:?}", ServeOptions::default());
    assert!(
        without_sink.contains("diagnostics: None"),
        "the no-sink default renders honestly: {without_sink}"
    );
}

#[test]
fn every_serve_error_variant_renders_its_own_remediation() {
    // Each refusal/overlay wording is part of the contract (the CLI maps
    // them onto the exit-code table; the operator reads them verbatim): a
    // held lock names the holder pid or the unreadable record, an invalid
    // invocation says its message as-is, and a startup failure says the
    // host could not start.
    let locked_with_holder = ServeError::StateDirLocked {
        lock_path: PathBuf::from("/srv/hekma/host.lock"),
        holder: Some(4242),
    };
    let text = locked_with_holder.to_string();
    assert!(
        text.contains("/srv/hekma/host.lock")
            && text.contains("4242")
            && text.contains("already served by another host"),
        "the held-lock refusal names the path AND the holder pid: {text}"
    );
    assert!(
        text.contains("stop that host first") && text.contains("serve a different state directory"),
        "the remediation rides along: {text}"
    );
    let locked_unreadable = ServeError::StateDirLocked {
        lock_path: PathBuf::from("/srv/hekma/host.lock"),
        holder: None,
    };
    let text = locked_unreadable.to_string();
    assert!(
        text.contains("/srv/hekma/host.lock") && text.contains("could not be read"),
        "an unreadable holder record says so instead of guessing a pid: {text}"
    );
    let invalid = ServeError::InvalidOptions {
        message: "invalid stop window 0".to_string(),
    };
    assert_eq!(invalid.to_string(), "invalid stop window 0");
    let startup = ServeError::Startup {
        message: "boom".to_string(),
    };
    assert_eq!(startup.to_string(), "the host could not start: boom");
}

#[test]
fn an_out_of_bounds_stop_window_is_refused_before_anything_is_touched() {
    // Review finding 9: the window is validated BEFORE any path, lock, or
    // engine work — a sub-second or multi-day window is a rejected
    // INVOCATION (`InvalidOptions`) that leaves the state root untouched,
    // and with a caller sink the refusal is ALSO noted through it (AI-18).
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, sink) = SharedSink::new();
    for bad in [0, hekma_host::MAX_STOP_WINDOW_SECS + 1] {
        let options = ServeOptions {
            state_base: Some(state.path().to_path_buf()),
            stop_window: Some(bad),
            diagnostics: Some(sink.clone()),
            ..ServeOptions::default()
        };
        let err = serve_with_signal(options, immediate_signal()).expect_err("refused");
        match err {
            ServeError::InvalidOptions { message } => {
                assert!(
                    message.contains(&format!("invalid stop window {bad}")),
                    "the rejection names the value: {message}"
                );
                assert!(
                    message.contains(&hekma_host::MIN_STOP_WINDOW_SECS.to_string())
                        && message.contains(&hekma_host::MAX_STOP_WINDOW_SECS.to_string()),
                    "the rejection names both bounds: {message}"
                );
            }
            other => panic!("expected InvalidOptions, got {other:?}"),
        }
    }
    assert!(
        sink_handle.text().contains("invalid stop window"),
        "with a caller sink the refusal is ALSO noted through it:\n{}",
        sink_handle.text()
    );
    assert!(
        !state.path().join("state.db").exists()
            && !state
                .path()
                .join(hekma_engine::paths::HOST_LOCK_FILE)
                .exists(),
        "a rejected invocation touches nothing"
    );
}

#[test]
fn a_state_dir_whose_parent_is_a_regular_file_fails_to_start_honestly() {
    // A state base that cannot exist (its parent is a file) is a Startup
    // failure naming the directory — the host says why it could not start
    // instead of unwrapping deep inside the open.
    let state = tempfile::tempdir().unwrap();
    let not_a_dir = state.path().join("not-a-dir");
    std::fs::write(&not_a_dir, b"a regular file").unwrap();
    let options = ServeOptions {
        state_base: Some(not_a_dir.join("inner")),
        ..ServeOptions::default()
    };
    let err = serve_with_signal(options, immediate_signal()).expect_err("cannot start");
    match err {
        ServeError::Startup { message } => assert!(
            message.contains("could not create the state directory"),
            "the failure names the state directory"
        ),
        other => panic!("expected Startup, got {other:?}"),
    }
}

#[test]
fn a_lock_path_that_cannot_be_a_file_surfaces_the_io_failure() {
    // A `host.lock` path occupied by a directory can never be locked: the
    // refusal must come back as the typed `Startup` (lock IO class) naming
    // the path — noted through the caller sink too (AI-18) — never a panic
    // or an engine opened alongside.
    let state = tempfile::tempdir().unwrap();
    std::fs::create_dir(state.path().join(hekma_engine::paths::HOST_LOCK_FILE)).unwrap();
    let (sink_handle, sink) = SharedSink::new();
    let options = ServeOptions {
        state_base: Some(state.path().to_path_buf()),
        diagnostics: Some(sink),
        ..ServeOptions::default()
    };
    let err = serve_with_signal(options, immediate_signal()).expect_err("cannot lock");
    match err {
        ServeError::Startup { message } => assert!(
            message.contains("could not acquire the host lock"),
            "the failure names the lock acquisition"
        ),
        other => panic!("expected Startup, got {other:?}"),
    }
    assert!(
        sink_handle
            .text()
            .contains("could not acquire the host lock"),
        "the lock IO failure is surfaced through the sink:\n{}",
        sink_handle.text()
    );
    assert!(
        !state.path().join("state.db").exists(),
        "a lock-IO refusal must not open an engine"
    );
}

#[test]
fn a_listen_without_a_bearer_secret_is_refused_and_noted_through_the_sink() {
    // The half-configured Control API (a listen address with no bearer
    // secret) is a rejected INVOCATION — the API never serves
    // unauthenticated and never half-configured. The CLI suite pins the
    // BINARY surface (the exit classes); this pins the LIBRARY surface: the
    // typed `InvalidOptions` naming both flags, the same refusal noted
    // through the caller sink (AI-18), and a rejected invocation touching
    // nothing (no lock file — validation runs before any state work).
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, sink) = SharedSink::new();
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let options = ServeOptions {
        state_base: Some(state.path().to_path_buf()),
        control_listen: Some(addr),
        bearer_secret: None,
        diagnostics: Some(sink),
        ..ServeOptions::default()
    };
    let err = serve_with_signal(options, immediate_signal()).expect_err("a half-configured API");
    match err {
        ServeError::InvalidOptions { message } => assert!(
            message.contains("--listen 127.0.0.1:0 requires --bearer-secret")
                && message.contains("never serves unauthenticated"),
            "the refusal names the flag pair and the reason: {message}"
        ),
        other => panic!("expected InvalidOptions, got {other:?}"),
    }
    assert!(
        sink_handle
            .text()
            .contains("--listen 127.0.0.1:0 requires --bearer-secret"),
        "with a caller sink the refusal is ALSO noted through it:\n{}",
        sink_handle.text()
    );
    assert!(
        !state
            .path()
            .join(hekma_engine::paths::HOST_LOCK_FILE)
            .exists(),
        "a rejected invocation touches nothing"
    );
}

#[test]
fn a_bearer_secret_without_a_listen_is_refused_and_noted_through_the_sink() {
    // The mirror half-configuration (a secret name with the API off) is the
    // same rejected-invocation class: there is no API to guard, and the
    // refusal says so naming both flags — through the error AND the caller
    // sink (AI-18), touching nothing.
    let state = tempfile::tempdir().unwrap();
    let (sink_handle, sink) = SharedSink::new();
    let options = ServeOptions {
        state_base: Some(state.path().to_path_buf()),
        control_listen: None,
        bearer_secret: Some("any-secret-name".to_string()),
        diagnostics: Some(sink),
        ..ServeOptions::default()
    };
    let err = serve_with_signal(options, immediate_signal()).expect_err("a half-configured API");
    match err {
        ServeError::InvalidOptions { message } => assert!(
            message.contains("--bearer-secret requires --listen")
                && message.contains("no API to guard"),
            "the refusal names the flag pair and the reason: {message}"
        ),
        other => panic!("expected InvalidOptions, got {other:?}"),
    }
    assert!(
        sink_handle
            .text()
            .contains("--bearer-secret requires --listen"),
        "with a caller sink the refusal is ALSO noted through it:\n{}",
        sink_handle.text()
    );
    assert!(
        !state
            .path()
            .join(hekma_engine::paths::HOST_LOCK_FILE)
            .exists(),
        "a rejected invocation touches nothing"
    );
}

#[test]
fn a_relative_state_dir_env_is_refused_as_a_startup_failure() {
    // `state_base: None` routes resolution through the engine's env order;
    // a RELATIVE `HEKMA_STATE_DIR` is the engine's typed refusal (a
    // relative base would bake CWD-relative paths into the stored Agent
    // Home paths). The host surfaces it as its OWN typed Startup, before
    // any lock or state-dir work. This is the only env reader in this test
    // binary (every other test passes an explicit base); the value is
    // saved and restored around the call, before any assert can fail.
    let saved = std::env::var_os(hekma_engine::paths::STATE_DIR_ENV_ALIAS);
    std::env::set_var(hekma_engine::paths::STATE_DIR_ENV_ALIAS, "relative-state");
    let err = serve_with_signal(ServeOptions::default(), immediate_signal())
        .expect_err("a relative env base must be refused");
    match saved {
        Some(value) => std::env::set_var(hekma_engine::paths::STATE_DIR_ENV_ALIAS, value),
        None => std::env::remove_var(hekma_engine::paths::STATE_DIR_ENV_ALIAS),
    }
    match err {
        ServeError::Startup { message } => assert!(
            message.contains("must be an absolute path") && message.contains("relative-state"),
            "the refusal names the value and the rule: {message}"
        ),
        other => panic!("expected Startup, got {other:?}"),
    }
}

#[test]
fn a_state_db_that_cannot_open_surfaces_as_the_typed_startup() {
    // A state root whose engine database can never open (here: `state.db`
    // occupied by a directory — sqlite cannot open it) is the host's typed
    // Startup, never an unwrap deep inside the open. Ordering proof: the
    // refusal happens AFTER the host lock (the lock file exists) and
    // BEFORE any supervision work — no engine was opened, so no drain had
    // anything to do.
    let state = tempfile::tempdir().unwrap();
    std::fs::create_dir(state.path().join(hekma_engine::paths::STATE_DB_FILE)).unwrap();
    let options = ServeOptions {
        state_base: Some(state.path().to_path_buf()),
        ..ServeOptions::default()
    };
    let err = serve_with_signal(options, immediate_signal()).expect_err("cannot open the engine");
    match err {
        ServeError::Startup { message } => assert!(
            message.contains("database"),
            "the engine's open failure surfaces through the Startup: {message}"
        ),
        other => panic!("expected Startup, got {other:?}"),
    }
    assert!(
        state
            .path()
            .join(hekma_engine::paths::HOST_LOCK_FILE)
            .exists(),
        "the lock ran first — the engine-open failure is a LATER refusal"
    );
}

/// A signal factory whose registration fails DURING the arming window (the
/// wait task resolves `Err` before the engine opens — review finding 2's
/// typed-Startup path).
async fn early_failing_wait(_sink: hekma_engine::DiagnosticSink) -> Result<&'static str, String> {
    Err(String::from("early registration failure: test"))
}

#[test]
fn a_registration_failure_during_arming_is_a_typed_startup_error() {
    // The typed-Startup contract (review finding 2): a registration failure
    // surfaces verbatim, never a panic. Whether the arming handshake or the
    // post-open wait observes the resolved `Err` is scheduler timing — the
    // surfaced error is the same either way, so this test pins ONLY the
    // contract (the deterministic post-open discriminators live in the
    // late-arm tests below).
    let state = tempfile::tempdir().unwrap();
    let options = ServeOptions {
        state_base: Some(state.path().to_path_buf()),
        ..ServeOptions::default()
    };
    let err = serve_with_signal(options, early_failing_wait).expect_err("registration failure");
    match err {
        ServeError::Startup { message } => assert_eq!(
            message, "early registration failure: test",
            "the registration failure surfaces verbatim"
        ),
        other => panic!("expected Startup, got {other:?}"),
    }
}

/// A signal factory that dies only AFTER the arming window (it sleeps past
/// the arming handshake, the engine opens, and only then the wait task
/// panics) — the post-open JoinError arm.
async fn late_panicking_wait(_sink: hekma_engine::DiagnosticSink) -> Result<&'static str, String> {
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    panic!("the shutdown-signal wait task died in a test");
}

#[test]
fn a_dead_signal_wait_task_after_the_open_is_surfaced_as_a_startup_error() {
    // A wait task that dies AFTER the engine opened must still surface as
    // the typed Startup (the JoinError wording, never a hang) — and the
    // state store proves this ran the POST-open arm, the discriminator from
    // the pre-open contract test above. Silence the deliberate panic's
    // default hook output (the ASSERTION is the surfaced error, not the
    // panic printer), then restore it.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let state = tempfile::tempdir().unwrap();
    let options = ServeOptions {
        state_base: Some(state.path().to_path_buf()),
        ..ServeOptions::default()
    };
    let err = serve_with_signal(options, late_panicking_wait).expect_err("dead wait task");
    std::panic::set_hook(previous_hook);
    match err {
        ServeError::Startup { message } => assert!(
            message.contains("wait task failed"),
            "the JoinError is surfaced honestly"
        ),
        other => panic!("expected Startup, got {other:?}"),
    }
    assert!(
        state.path().join("state.db").exists(),
        "the post-open arm runs AFTER the open — the discriminator from the pre-open contract"
    );
}

/// A signal factory that fails only AFTER the arming window (it sleeps past
/// the arming handshake, the engine opens, and only then resolves `Err`).
async fn late_failing_wait(_sink: hekma_engine::DiagnosticSink) -> Result<&'static str, String> {
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    Err(String::from("late registration failure: test"))
}

#[test]
fn a_registration_failure_after_the_engine_opened_is_still_typed_startup() {
    // The LATE arm of the same contract: the armed wait resolves Err after
    // the open. The discriminator from the early arm is the state store —
    // here the engine DID open (the failure is post-open), and the typed
    // Startup still carries the verbatim registration failure.
    let state = tempfile::tempdir().unwrap();
    let options = ServeOptions {
        state_base: Some(state.path().to_path_buf()),
        ..ServeOptions::default()
    };
    let err = serve_with_signal(options, late_failing_wait).expect_err("late failure");
    match err {
        ServeError::Startup { message } => assert_eq!(
            message, "late registration failure: test",
            "the late registration failure surfaces verbatim"
        ),
        other => panic!("expected Startup, got {other:?}"),
    }
    assert!(
        state.path().join("state.db").exists(),
        "the late arm runs AFTER the open — the discriminator from the early arm"
    );
}

#[test]
fn a_poisoned_sink_never_silences_the_refusal_notice() {
    // Review finding 17: a sink mutex POISONED by a panicking writer is
    // recovered (`into_inner`) — a torn stream must not silence later host
    // notices. The invalid-window refusal (which notes through the sink)
    // still lands its line even on the poisoned sink.
    let (sink_handle, sink) = SharedSink::new();
    let to_poison = sink.clone();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Hold the sink's mutex while panicking: the guard drops during the
        // unwind and the mutex poisons — exactly the mid-write panic shape.
        let _held = to_poison.lock().unwrap();
        std::panic::panic_any("deliberately poison the sink mutex");
    }));
    std::panic::set_hook(previous_hook);
    let state = tempfile::tempdir().unwrap();
    let options = ServeOptions {
        state_base: Some(state.path().to_path_buf()),
        stop_window: Some(0),
        diagnostics: Some(sink),
        ..ServeOptions::default()
    };
    let err = serve_with_signal(options, immediate_signal()).expect_err("refused");
    assert!(
        matches!(err, ServeError::InvalidOptions { .. }),
        "the refusal itself is unchanged by the poisoned sink: {err:?}"
    );
    assert!(
        sink_handle.text().contains("invalid stop window"),
        "the poisoned sink was recovered and the notice still landed:\n{}",
        sink_handle.text()
    );
}
