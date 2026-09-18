//! Story 10-2 (Epic 10): the host-provided diagnostic sink.
//!
//! The engine's TWO operational diagnostics — the DC-10 memory-delivery
//! notice and the enforcement breadcrumb — route through a host-provided
//! [`DiagnosticSink`](hemaka_engine::DiagnosticSink) when one is installed
//! (at open via `Engine::open_with_diagnostics`, or any time later via
//! `Engine::with_diagnostics` / `Blocking::with_diagnostics`), and keep their
//! historical stderr behavior when none is. These tests pin the whole
//! acceptance surface:
//!
//! * **Sink receives BOTH diagnostics' exact texts** — driven through the
//!   REAL production paths, not synthetic emissions: the notice via the
//!   attach-unmapped-manifest-then-start path (the DC-10 fault-install), and
//!   the breadcrumb via a budget breach with `breach_action = pause` on an
//!   instance the token breach has ALREADY paused (the dollar dimension then
//!   fails to enforce a second pause — the exact production path the 7-1
//!   suites exercise; the enforcement site evaluates the TOKEN ceilings
//!   first, the pause commits synchronously in the same lock pass, and the
//!   dollar breach's `enforce_pause` hits the already-paused transition
//!   gate). The expected lines are computed from the run itself (the
//!   engine-reported managed dir; the SAME transition-gate error the
//!   enforcement path received, reproduced through the facade), so the
//!   assertions are byte-exact without hardcoding an error string.
//! * **No-sink default is byte-identical to today** — a subprocess re-exec of
//!   this test binary (the `adoption.rs` pattern; std cannot capture a
//!   process's own stderr) runs the SAME flow with no sink and the parent
//!   asserts the child's captured stderr carries exactly the two `[hemaka]`
//!   lines and nothing else — same wording, same stream, one line each.
//! * **With a sink installed, stderr stays silent** — the sink-mode child
//!   drives the same flow with the sink installed; the parent asserts the
//!   child's stderr contains no `[hemaka]` line at all while the sink's
//!   captured bytes (relayed via a file) equal the expected pair exactly.
//! * **The contract corners the docs promise** (the Epic-10 hardening):
//!   mid-flight ROTATION splits the diagnostics across the two sinks (sink A
//!   holds exactly the first line, sink B exactly the second — installing
//!   REPLACES, which an `if diagnostics.is_none()`-shaped install would
//!   silently break); a best-effort write (an always-failing writer is
//!   swallowed and supervision continues); panic recovery (a writer that
//!   panics on its FIRST write is caught — the supervisor mutex never
//!   poisons — and the same sink receives the next diagnostic); and the
//!   shared-Arc shape (one sink `Arc` across TWO engines, both diagnostics
//!   landing in the one capture in call order).
//!
//! Determinism posture (the house style): every wait polls COMMITTED state
//! (the shared `uj3` bounded poller) or the sink's own captured bytes — never
//! a wall-clock guess. No `OsId` gate; runs unmodified on all three OSes.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hemaka_conformance::test_support::ManifestFixture;
use hemaka_conformance::uj3;
use hemaka_engine::{
    AdapterRef, DiagnosticSink, Engine, EngineError, LifecycleState, MemoryBackingKind,
};
use tempfile::TempDir;

/// The instance that fires the DC-10 memory-delivery notice (an unmapped
/// manifest adapter with a `filesystem` backing attached).
const NOTICE_INSTANCE: &str = "svc";

/// The instance that fires the enforcement breadcrumb (the uj3 flow: token
/// ceiling AND dollar cap, both with `breach_action = pause`).
const BREADCRUMB_INSTANCE: &str = "flow-probe";

/// The shared bounded budget for committed-state / sink-content polls.
const POLL_BUDGET: Duration = Duration::from_secs(30);

/// Env var turning the re-exec'd test binary into the helper child (the
/// `adoption.rs` pattern). Value: `default` or `sink`.
const HELPER_ENV: &str = "KTESIO_DIAG_SINK_HELPER";
/// Env var: the child's engine state root (created by the child).
const STATE_ENV: &str = "KTESIO_DIAG_SINK_STATE";
/// Env var: where the child writes the two EXPECTED diagnostic lines.
const EXPECTED_ENV: &str = "KTESIO_DIAG_SINK_EXPECTED";
/// Env var: where the sink-mode child relays the sink's captured bytes.
const CAPTURED_ENV: &str = "KTESIO_DIAG_SINK_CAPTURED";

// ---------------------------------------------------------------------------
// The shared sink capture + the shared two-diagnostic flow
// ---------------------------------------------------------------------------

/// A `Write` adapter delegating into a shared buffer, so the test can read the
/// sink's captured bytes while the engine owns the boxed writer. This mirrors
/// the real host shape: a sink that routes lines into state shared with the
/// rest of the host process.
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

/// Build the engine-named sink handle over a shared capture buffer.
fn make_sink(shared: &Arc<Mutex<Vec<u8>>>) -> DiagnosticSink {
    Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(shared)))))
}

/// The unmapped manifest fixture: a `fake_agent` manifest with NO `[config]`
/// section — no target for the reserved `memory.dir` key, the DC-10 notice's
/// trigger — lingering so the instance runs. Same shape as `memory.rs`'s
/// unmapped fixture (story 10-1's shared builder).
fn write_unmapped_manifest(dir: &Path) -> PathBuf {
    ManifestFixture::fake_agent(NOTICE_INSTANCE, &["--linger-ms", "600000"]).write(dir)
}

/// The exact expected DC-10 notice LINE for an engine-reported managed dir
/// (the choke point emits `[hemaka] ` + this text + a terminating '\n').
/// Shared by the byte-exact suites and the rotation/Arc-sharing suites.
fn expected_notice_line(dir: &Path) -> String {
    format!(
        "[hemaka] {NOTICE_INSTANCE}: a 'filesystem' Memory Backing is attached (managed \
         directory: {}), but this adapter declares no config mapping for the reserved key \
         'memory.dir', so the agent will NOT receive the path. Add [config.\"memory.dir\"] \
         env = \"...\" to its manifest to deliver it.",
        dir.display()
    )
}

/// The exact expected enforcement-breadcrumb LINE for the transition-gate
/// error the enforcement path received (same emission shape as above).
fn expected_breadcrumb_line(pause_err: &EngineError) -> String {
    format!("[hemaka] {BREADCRUMB_INSTANCE}: budget breach pause could not be honored: {pause_err}")
}

/// Drive BOTH diagnostics through their real production paths on ONE engine
/// and return `(notice_line, breadcrumb_line, breadcrumb_needle)` — the exact
/// expected diagnostic LINES (without the trailing newlines the emitter
/// appends) plus the fixed needle sink-mode callers poll for:
///
/// 1. the DC-10 notice — register the unmapped manifest adapter, attach a
///    `filesystem` backing, start (the start still SUCCEEDS and the notice
///    fires, naming the engine-reported managed dir);
/// 2. the enforcement breadcrumb — the uj3 flow (token ceiling + dollar cap,
///    `breach_action = pause`): the token breach pauses the instance, and the
///    dollar breach's `enforce_pause` then fails on the already-paused gate —
///    the exact production path from 7-1's suites. The expected line's
///    `{detail}` is the SAME transition-gate error the enforcement path
///    received, reproduced by pausing the already-paused instance through the
///    facade (a gate rejection in both paths, no side effect either way).
fn drive_both_diagnostics(
    engine: &Engine,
    state: &Path,
    unmapped: &Path,
    flow: &Path,
) -> (String, String, String) {
    let facade = engine.blocking();

    // ---- (1) The DC-10 memory-delivery notice. ----
    facade
        .register_with_adapter(
            NOTICE_INSTANCE,
            &AdapterRef::Manifest(unmapped.to_path_buf()),
        )
        .expect("register the unmapped manifest instance");
    let dir = facade
        .attach_memory(NOTICE_INSTANCE, MemoryBackingKind::Filesystem)
        .expect("attach the filesystem backing");
    let started = facade
        .start(NOTICE_INSTANCE)
        .expect("the unmapped start STILL succeeds");
    assert_eq!(started.state, LifecycleState::Running);
    let notice_line = expected_notice_line(&dir);

    // ---- (2) The enforcement breadcrumb. ----
    facade
        .register_with_adapter(
            BREADCRUMB_INSTANCE,
            &AdapterRef::Manifest(flow.to_path_buf()),
        )
        .expect("register the flow instance");
    for (key, value) in uj3::flow_config_pairs() {
        facade
            .set_config(BREADCRUMB_INSTANCE, key, value)
            .unwrap_or_else(|e| panic!("set_config {key}={value} failed: {e}"));
    }
    facade
        .start(BREADCRUMB_INSTANCE)
        .expect("start the flow instance");

    // The token breach pauses the instance (committed state, bounded poll);
    // the dollar breach's failed pause rides the same supervisor lock pass.
    uj3::wait_for_state(
        state,
        BREADCRUMB_INSTANCE,
        LifecycleState::Paused,
        POLL_BUDGET,
    );

    // Wait for the breadcrumb to actually ARRIVE (committed-state polling
    // cannot see it — it is one lock pass after the pause commit). `needle` is
    // the breadcrumb's fixed middle; the exact line is asserted by the caller.
    let breadcrumb_needle = "budget breach pause could not be honored:";

    // The expected line, computed from the SAME gate error the enforcement
    // hit. This facade call is also the determinism barrier: it takes the
    // supervisor lock, which the ingestion pass holds through the breadcrumb
    // emission — so once it returns, the diagnostic is already out (this is
    // what makes the no-sink child's exit safe: no diagnostic can still be in
    // flight when the engine drops).
    let pause_err = facade
        .pause(BREADCRUMB_INSTANCE)
        .expect_err("pausing an already-paused instance is a gate rejection");
    let breadcrumb_line = expected_breadcrumb_line(&pause_err);

    // Teardown (never mask the assertions): the flow instance is SIGSTOP'd
    // (Unix), so its stop takes the shared zero window.
    let _ = facade.stop(NOTICE_INSTANCE, Some(Duration::from_secs(5)));
    let _ = facade.stop(BREADCRUMB_INSTANCE, Some(uj3::STOP_WINDOW));

    (notice_line, breadcrumb_line, breadcrumb_needle.to_string())
}

/// Poll the sink's captured bytes until `needle` arrives (bounded) and return
/// the full capture.
fn wait_for_capture(shared: &Arc<Mutex<Vec<u8>>>, needle: &str) -> Vec<u8> {
    let deadline = Instant::now() + POLL_BUDGET;
    loop {
        let bytes = shared.lock().unwrap().clone();
        if String::from_utf8_lossy(&bytes).contains(needle) {
            return bytes;
        }
        assert!(
            Instant::now() < deadline,
            "the enforcement breadcrumb never reached the sink (captured so far: {})",
            String::from_utf8_lossy(&bytes)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// Sink-installed engines (in process): both diagnostics, exact texts
// ---------------------------------------------------------------------------

#[test]
fn an_open_time_sink_receives_both_diagnostics_exact_texts() {
    // `Engine::open_with_diagnostics`: the airtight install — the sink is in
    // place before any supervision work. Both diagnostics must arrive as the
    // EXACT expected lines, in order, and NOTHING else: the capture equals the
    // two lines byte-for-byte (any extra engine diagnostic would corrupt it).
    let state = TempDir::new().expect("state root");
    let unmapped_dir = TempDir::new().expect("unmapped manifest dir");
    let flow_dir = TempDir::new().expect("flow manifest dir");
    let unmapped = write_unmapped_manifest(unmapped_dir.path());
    let flow = uj3::write_flow_manifest(flow_dir.path());

    let captured = Arc::new(Mutex::new(Vec::new()));
    let engine =
        Engine::open_with_diagnostics(Some(state.path().to_path_buf()), make_sink(&captured))
            .expect("open engine with sink");

    let (notice_line, breadcrumb_line, needle) =
        drive_both_diagnostics(&engine, state.path(), &unmapped, &flow);
    let bytes = wait_for_capture(&captured, &needle);
    assert_eq!(
        bytes,
        format!("{notice_line}\n{breadcrumb_line}\n").into_bytes(),
        "the sink must receive EXACTLY the two diagnostic lines (in emission order)"
    );
}

#[test]
fn a_facade_installed_sink_receives_both_diagnostics_exact_texts() {
    // The post-open install path: `Engine::open` (no sink — the default), then
    // `Blocking::with_diagnostics` BEFORE any instance starts. The engine must
    // behave identically to the open-time install from that point on.
    let state = TempDir::new().expect("state root");
    let unmapped_dir = TempDir::new().expect("unmapped manifest dir");
    let flow_dir = TempDir::new().expect("flow manifest dir");
    let unmapped = write_unmapped_manifest(unmapped_dir.path());
    let flow = uj3::write_flow_manifest(flow_dir.path());

    let captured = Arc::new(Mutex::new(Vec::new()));
    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    engine.blocking().with_diagnostics(make_sink(&captured));

    let (notice_line, breadcrumb_line, needle) =
        drive_both_diagnostics(&engine, state.path(), &unmapped, &flow);
    let bytes = wait_for_capture(&captured, &needle);
    assert_eq!(
        bytes,
        format!("{notice_line}\n{breadcrumb_line}\n").into_bytes(),
        "the facade-installed sink must receive EXACTLY the two diagnostic lines"
    );
}

// ---------------------------------------------------------------------------
// The contract corners the docs promise: rotation, best-effort writes,
// panic recovery, and the shared-Arc shape
// ---------------------------------------------------------------------------

#[test]
fn a_mid_flight_rotation_splits_the_diagnostics_between_the_two_sinks() {
    // The documented rotation contract — installing REPLACES, and the change
    // takes effect for every later diagnostic: sink A is in place for the
    // FIRST diagnostic, sink B replaces it mid-flight, the SECOND diagnostic
    // routes to B. A must hold exactly the first line (nothing after the
    // rotation) and B exactly the second. (This is the contract an
    // `if self.diagnostics.is_none()`-shaped install would silently break:
    // the second install would be a no-op and BOTH lines would land on A.)
    let state = TempDir::new().expect("state root");
    let unmapped_dir = TempDir::new().expect("unmapped manifest dir");
    let flow_dir = TempDir::new().expect("flow manifest dir");
    let unmapped = write_unmapped_manifest(unmapped_dir.path());
    let flow = uj3::write_flow_manifest(flow_dir.path());

    let captured_a = Arc::new(Mutex::new(Vec::new()));
    let captured_b = Arc::new(Mutex::new(Vec::new()));
    let engine = Engine::open(Some(state.path().to_path_buf())).expect("open engine");
    let facade = engine.blocking();

    // Sink A in place BEFORE any supervision work; the FIRST diagnostic
    // (the DC-10 notice) is emitted synchronously inside `start`.
    facade.with_diagnostics(make_sink(&captured_a));
    facade
        .register_with_adapter(
            NOTICE_INSTANCE,
            &AdapterRef::Manifest(unmapped.to_path_buf()),
        )
        .expect("register the unmapped manifest instance");
    let dir = facade
        .attach_memory(NOTICE_INSTANCE, MemoryBackingKind::Filesystem)
        .expect("attach the filesystem backing");
    let started = facade
        .start(NOTICE_INSTANCE)
        .expect("the unmapped start STILL succeeds");
    assert_eq!(started.state, LifecycleState::Running);
    let notice_line = expected_notice_line(&dir);

    // ROTATE mid-flight: sink B replaces A before the SECOND diagnostic.
    facade.with_diagnostics(make_sink(&captured_b));

    // The SECOND diagnostic rides the same production path as the byte-exact
    // suites above. The `pause` call is the determinism barrier (it takes the
    // supervisor lock the ingestion pass holds through the emission).
    facade
        .register_with_adapter(
            BREADCRUMB_INSTANCE,
            &AdapterRef::Manifest(flow.to_path_buf()),
        )
        .expect("register the flow instance");
    for (key, value) in uj3::flow_config_pairs() {
        facade
            .set_config(BREADCRUMB_INSTANCE, key, value)
            .unwrap_or_else(|e| panic!("set_config {key}={value} failed: {e}"));
    }
    facade
        .start(BREADCRUMB_INSTANCE)
        .expect("start the flow instance");
    uj3::wait_for_state(
        state.path(),
        BREADCRUMB_INSTANCE,
        LifecycleState::Paused,
        POLL_BUDGET,
    );
    let pause_err = facade
        .pause(BREADCRUMB_INSTANCE)
        .expect_err("pausing an already-paused instance is a gate rejection");
    let breadcrumb_line = expected_breadcrumb_line(&pause_err);

    // B holds EXACTLY the post-rotation diagnostic…
    let bytes_b = wait_for_capture(&captured_b, "budget breach pause could not be honored:");
    assert_eq!(
        bytes_b,
        format!("{breadcrumb_line}\n").into_bytes(),
        "the post-rotation sink holds exactly the second diagnostic"
    );
    // …and A holds ONLY the first (nothing leaked past the rotation).
    assert_eq!(
        *captured_a.lock().unwrap(),
        format!("{notice_line}\n").into_bytes(),
        "the pre-rotation sink holds exactly the first diagnostic and nothing after"
    );

    // Teardown (never mask the assertions).
    let _ = facade.stop(NOTICE_INSTANCE, Some(Duration::from_secs(5)));
    let _ = facade.stop(BREADCRUMB_INSTANCE, Some(uj3::STOP_WINDOW));
}

/// A sink writer whose EVERY write fails (the closed/broken host writer the
/// best-effort contract names). `Send` so it boxes into a [`DiagnosticSink`].
struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("the host sink is closed"))
    }

    fn write_all(&mut self, _buf: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other("the host sink is closed"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("the host sink is closed"))
    }
}

#[test]
fn a_failing_sink_write_is_swallowed_and_supervision_continues() {
    // Best-effort by contract: a broken or closed host writer must never
    // fail, block, or crash supervision. With an always-failing sink
    // installed FROM OPEN, the diagnostic-emitting start still SUCCEEDS, the
    // instance reaches its committed state, and the engine keeps working
    // (a later stop succeeds) — the swallowed write is invisible to
    // supervision.
    let state = TempDir::new().expect("state root");
    let unmapped_dir = TempDir::new().expect("unmapped manifest dir");
    let unmapped = write_unmapped_manifest(unmapped_dir.path());

    let engine = Engine::open_with_diagnostics(
        Some(state.path().to_path_buf()),
        Arc::new(Mutex::new(Box::new(FailingWriter))),
    )
    .expect("open engine with a failing sink");
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            NOTICE_INSTANCE,
            &AdapterRef::Manifest(unmapped.to_path_buf()),
        )
        .expect("register the unmapped manifest instance");
    facade
        .attach_memory(NOTICE_INSTANCE, MemoryBackingKind::Filesystem)
        .expect("attach the filesystem backing");
    let started = facade
        .start(NOTICE_INSTANCE)
        .expect("the diagnostic write failure is swallowed — start succeeds");
    assert_eq!(started.state, LifecycleState::Running);

    // The engine keeps supervising after the swallowed failure.
    let stopped = facade
        .stop(NOTICE_INSTANCE, Some(Duration::from_secs(5)))
        .expect("supervision continues after the swallowed write failure");
    assert_eq!(stopped.state, LifecycleState::Stopped);
}

/// A sink writer that PANICS on its FIRST write and works afterwards — the
/// panic-recovery corner: `emit_diagnostic` catches the panic (a host bug
/// must never unwind through the supervisor's critical section — that would
/// poison the supervisor mutex — nor poison the sink's own mutex), and the
/// SAME sink keeps receiving later diagnostics.
struct PanicOnceWriter {
    panicked: bool,
    captured: Arc<Mutex<Vec<u8>>>,
}

impl PanicOnceWriter {
    fn write_once(&mut self, buf: &[u8]) {
        if !self.panicked {
            self.panicked = true;
            panic!("the host writer panicked mid-write");
        }
        self.captured.lock().unwrap().extend_from_slice(buf);
    }
}

impl Write for PanicOnceWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write_once(buf);
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.write_once(buf);
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_panicking_sink_write_is_caught_and_the_same_sink_keeps_receiving() {
    // The first write PANICS inside the engine's emission — caught and
    // swallowed (`catch_unwind` keeps it from unwinding through the
    // supervisor-lock critical section, so neither the supervisor mutex nor
    // the sink's own mutex poisons). The SECOND diagnostic then lands on the
    // SAME sink, proving recovery is real and not a poisoned-mutex-only
    // `into_inner` salvage.
    let state = TempDir::new().expect("state root");
    let unmapped_dir = TempDir::new().expect("unmapped manifest dir");
    let flow_dir = TempDir::new().expect("flow manifest dir");
    let unmapped = write_unmapped_manifest(unmapped_dir.path());
    let flow = uj3::write_flow_manifest(flow_dir.path());

    let captured = Arc::new(Mutex::new(Vec::new()));
    let engine = Engine::open_with_diagnostics(
        Some(state.path().to_path_buf()),
        Arc::new(Mutex::new(Box::new(PanicOnceWriter {
            panicked: false,
            captured: Arc::clone(&captured),
        }))),
    )
    .expect("open engine with a panic-once sink");

    let (notice_line, breadcrumb_line, needle) =
        drive_both_diagnostics(&engine, state.path(), &unmapped, &flow);
    let bytes = wait_for_capture(&captured, &needle);
    assert_eq!(
        bytes,
        format!("{breadcrumb_line}\n").into_bytes(),
        "the panicking FIRST write is swallowed (the notice is lost, best-effort) and the \
         SECOND diagnostic lands on the same sink: capture must be exactly the breadcrumb"
    );
    assert!(
        !bytes
            .windows(notice_line.len())
            .any(|w| w == notice_line.as_bytes()),
        "the notice whose write panicked never partially reached the sink"
    );
}

#[test]
fn one_shared_sink_arc_serves_two_engines_in_one_process() {
    // The `DiagnosticSink` name is `Arc<Mutex<Box<dyn Write + Send>>>` so a
    // host can clone the Arc and share ONE sink across engines (the type's
    // documented shape): two engines over DIFFERENT hermetic roots, the same
    // Arc — each engine's diagnostic lands in the one shared capture, in
    // call order (each emission is synchronous with its facade call and
    // serialized by the sink's mutex).
    let state_a = TempDir::new().expect("engine-A state root");
    let state_b = TempDir::new().expect("engine-B state root");
    let unmapped_dir_a = TempDir::new().expect("engine-A manifest dir");
    let unmapped_dir_b = TempDir::new().expect("engine-B manifest dir");
    let unmapped_a = write_unmapped_manifest(unmapped_dir_a.path());
    let unmapped_b = write_unmapped_manifest(unmapped_dir_b.path());

    let captured = Arc::new(Mutex::new(Vec::new()));
    let shared = make_sink(&captured);
    let engine_a = Engine::open_with_diagnostics(
        Some(state_a.path().to_path_buf()),
        std::sync::Arc::clone(&shared),
    )
    .expect("open engine A with the shared sink");
    let engine_b = Engine::open_with_diagnostics(
        Some(state_b.path().to_path_buf()),
        std::sync::Arc::clone(&shared),
    )
    .expect("open engine B with the SAME sink Arc");

    // Fire the notice on each engine; both land in the shared capture.
    let notice_line_a;
    let notice_line_b;
    let needle_b;
    {
        let facade = engine_a.blocking();
        facade
            .register_with_adapter(
                NOTICE_INSTANCE,
                &AdapterRef::Manifest(unmapped_a.to_path_buf()),
            )
            .expect("register the unmapped manifest instance on engine A");
        let dir = facade
            .attach_memory(NOTICE_INSTANCE, MemoryBackingKind::Filesystem)
            .expect("attach the filesystem backing on engine A");
        let started = facade.start(NOTICE_INSTANCE).expect("start on engine A");
        assert_eq!(started.state, LifecycleState::Running);
        notice_line_a = expected_notice_line(&dir);
        let _ = facade.stop(NOTICE_INSTANCE, Some(Duration::from_secs(5)));
    }
    {
        let facade = engine_b.blocking();
        facade
            .register_with_adapter(
                NOTICE_INSTANCE,
                &AdapterRef::Manifest(unmapped_b.to_path_buf()),
            )
            .expect("register the unmapped manifest instance on engine B");
        let dir = facade
            .attach_memory(NOTICE_INSTANCE, MemoryBackingKind::Filesystem)
            .expect("attach the filesystem backing on engine B");
        needle_b = dir.display().to_string();
        let started = facade.start(NOTICE_INSTANCE).expect("start on engine B");
        assert_eq!(started.state, LifecycleState::Running);
        notice_line_b = expected_notice_line(&dir);
        let _ = facade.stop(NOTICE_INSTANCE, Some(Duration::from_secs(5)));
    }

    // The SHARED capture holds BOTH engines' diagnostics, in call order.
    let bytes = wait_for_capture(&captured, &needle_b);
    assert_eq!(
        bytes,
        format!("{notice_line_a}\n{notice_line_b}\n").into_bytes(),
        "one shared sink Arc receives both engines' diagnostics, in call order"
    );
}

// ---------------------------------------------------------------------------
// The no-sink default (subprocess): stderr stays byte-identical to today
// ---------------------------------------------------------------------------

/// The re-exec helper (the `adoption.rs` pattern). With `KTESIO_DIAG_SINK_HELPER`
/// unset this is a no-op pass (the normal in-process run). In `default` mode it
/// drives the SAME two-diagnostic flow with NO sink (stderr inherits the test
/// runner's pipe — the parent captures it); in `sink` mode it installs the sink
/// and relays the captured bytes to the parent via a file. Both modes write the
/// EXPECTED lines to a file so the parent asserts byte-exact contents without
/// duplicating the engine-computed values (the managed dir, the gate error).
#[test]
fn diagnostic_sink_helper_subprocess() {
    let Ok(mode) = std::env::var(HELPER_ENV) else {
        return; // normal in-process invocation: nothing to do.
    };
    let state = PathBuf::from(std::env::var(STATE_ENV).unwrap());
    let expected_path = PathBuf::from(std::env::var(EXPECTED_ENV).unwrap());

    let unmapped_dir = TempDir::new().expect("unmapped manifest dir");
    let flow_dir = TempDir::new().expect("flow manifest dir");
    let unmapped = write_unmapped_manifest(unmapped_dir.path());
    let flow = uj3::write_flow_manifest(flow_dir.path());

    let (notice_line, breadcrumb_line, _needle) = match mode.as_str() {
        // No sink: the diagnostics go to the inherited stderr (the parent's
        // capture). The expected-lines file is the byte-exact assertion target.
        "default" => {
            let engine = Engine::open(Some(state.clone())).expect("open default engine");
            drive_both_diagnostics(&engine, &state, &unmapped, &flow)
        }
        // Sink installed at open: the diagnostics go to the sink; relay the
        // captured bytes to the parent and assert them here too (a child-side
        // failure fails the subprocess, which fails the parent).
        "sink" => {
            let captured = Arc::new(Mutex::new(Vec::new()));
            let engine = Engine::open_with_diagnostics(Some(state.clone()), make_sink(&captured))
                .expect("open sink engine");
            let outcome = drive_both_diagnostics(&engine, &state, &unmapped, &flow);
            let bytes = wait_for_capture(&captured, &outcome.2);
            assert_eq!(
                bytes,
                format!("{}\n{}\n", outcome.0, outcome.1).into_bytes(),
                "the child's sink must receive EXACTLY the two diagnostic lines"
            );
            let captured_path = PathBuf::from(std::env::var(CAPTURED_ENV).unwrap());
            std::fs::write(captured_path, bytes).expect("relay the captured sink bytes");
            (outcome.0, outcome.1, outcome.2)
        }
        other => panic!("unknown helper mode: {other}"),
    };

    std::fs::write(expected_path, format!("{notice_line}\n{breadcrumb_line}\n"))
        .expect("write the expected-lines file");
}

/// Run the helper child in `mode` and return (child stderr, expected-lines
/// path, captured-sink path).
fn run_helper(mode: &str, root: &Path) -> (String, PathBuf, PathBuf) {
    let exe = std::env::current_exe().expect("test exe");
    let state = root.join("state");
    let expected = root.join("expected.txt");
    let captured = root.join("captured.txt");
    let output = Command::new(exe)
        .args([
            "--exact",
            "diagnostic_sink_helper_subprocess",
            "--nocapture",
        ])
        .env(HELPER_ENV, mode)
        .env(STATE_ENV, &state)
        .env(EXPECTED_ENV, &expected)
        .env(CAPTURED_ENV, &captured)
        // `.output()` captures stderr into the result by default.
        .output()
        .expect("run the diagnostic-sink helper subprocess");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "the {mode}-mode helper subprocess failed: {status}; stderr={stderr}",
        status = output.status,
    );
    (stderr, expected, captured)
}

/// Read the two expected lines the child wrote (trailing newline stripped per
/// line for `contains`-style exact-line assertions).
fn read_expected_lines(expected: &Path) -> Vec<String> {
    String::from_utf8(std::fs::read(expected).expect("read the expected-lines file"))
        .expect("expected lines are UTF-8")
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn without_a_sink_both_diagnostics_reach_stderr_byte_identical() {
    // The acceptance row "given no sink … stderr output is byte-identical to
    // today's": the child runs the REAL flow with no sink; the parent asserts
    // the child's stderr carries EXACTLY the two `[hemaka]` diagnostic lines —
    // same wording, same stream, one line each, nothing else engine-emitted.
    let root = TempDir::new().expect("helper root");
    let (stderr, expected, _captured) = run_helper("default", root.path());
    let lines = read_expected_lines(&expected);
    assert_eq!(lines.len(), 2, "the child pins exactly two diagnostics");

    let engine_lines: Vec<&str> = stderr
        .lines()
        .filter(|l| l.starts_with("[hemaka]"))
        .collect();
    assert_eq!(
        engine_lines,
        lines.iter().map(String::as_str).collect::<Vec<_>>(),
        "the no-sink stderr must carry EXACTLY the two diagnostic lines \
         (byte-identical to the pre-sink engine): stderr=\n{stderr}"
    );
}

#[test]
fn with_a_sink_installed_stderr_stays_silent() {
    // The acceptance row "given a sink installed … stderr stays silent": the
    // child installs the sink AT OPEN and drives the same flow; the parent
    // asserts NO `[hemaka]` line reached the child's stderr while the relayed
    // sink bytes equal the expected pair exactly.
    let root = TempDir::new().expect("helper root");
    let (stderr, expected, captured) = run_helper("sink", root.path());
    let lines = read_expected_lines(&expected);
    assert_eq!(lines.len(), 2, "the child pins exactly two diagnostics");

    assert!(
        !stderr.contains("[hemaka]"),
        "with a sink installed the engine must not write the diagnostics to \
         stderr: stderr=\n{stderr}"
    );

    let relayed = std::fs::read(captured).expect("read the relayed sink capture");
    assert_eq!(
        relayed,
        format!("{}\n{}\n", lines[0], lines[1]).into_bytes(),
        "the relayed sink capture must equal the two expected lines exactly"
    );
}
