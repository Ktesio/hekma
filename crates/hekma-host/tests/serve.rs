//! Integration tests for the host's serve lifecycle (story 17-2): the full
//! [`serve_with_signal`] cycle on a hermetic state root, the lock refusal
//! (second host), and the no-instance drain — all driven through the PUBLIC
//! library surface with a caller-supplied sink, no OS signal needed (the
//! signal seam is the factory; the real OS-signal path is proven by the
//! `hekma serve` CLI end-to-end test and the in-crate signals test).

use std::io::Write;
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
