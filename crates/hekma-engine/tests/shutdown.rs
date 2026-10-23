//! Integration tests for the engine-owned shutdown drain (issue #328 item 3):
//! [`Engine::shutdown`] — the fleet-wide stop ladder a plain embedding calls
//! before dropping the engine — driven end-to-end through the PUBLIC
//! blocking facade, spawning the REAL `fake_agent`.
//!
//! These are the engine-side twins of the host's `drain_subprocess` legs
//! (which still prove the serve wiring through the delegate): the facade's
//! own legs pin what the HOST used to own — a running agent stops through
//! the ladder gracefully, a PAUSED agent gets the resume-then-stop ladder
//! (never the engine `Drop`'s raw kill-on-drop teardown — the #327 bug this
//! facade exists to spare embedders), and a quiescent fleet is an honest
//! no-op on the notice stream.
//!
//! ## Robust, cross-OS by construction
//!
//! No `OsId`-gated skip anywhere and no POSIX liveness probes: determinism
//! comes from asserting on COMMITTED lifecycle state and the returned
//! outcomes, never on process-suspension timing. The `fake_agent` handles
//! its stop signal on all three OSes (the lifecycle suite proves the ladder
//! cross-OS), and the fixture declares `pause` `guaranteed` on all OSes so
//! the committed `paused` state is deterministic everywhere.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hekma_engine::{AdapterRef, DiagnosticSink, DrainOutcome, Engine, LifecycleState, StopResult};

/// A sink that records every notice into a shared buffer the test can
/// assert on (the host suite's shape).
#[derive(Clone)]
struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl SharedSink {
    fn new() -> (Self, DiagnosticSink) {
        let shared = Self(Arc::new(Mutex::new(Vec::new())));
        let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(shared.clone())));
        (shared, sink)
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl std::io::Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Register + start a `fake_agent` (`--linger-ms` long enough to outlive the
/// drain window) and wait for its COMMITTED `running` state.
fn start_agent(engine: &Engine, manifest_dir: &Path, name: &str) {
    hekma_conformance::test_support::ManifestFixture::new(name, &["--linger-ms", "600000"])
        .guaranteed_on_all_oses("pause")
        .guaranteed_on_all_oses("interaction")
        .write(manifest_dir);
    let facade = engine.blocking();
    facade
        .register_with_adapter(name, &AdapterRef::Manifest(manifest_dir.to_path_buf()))
        .expect("register");
    facade.start(name).expect("start");
    wait_for_state(engine, name, LifecycleState::Running);
}

/// Poll the COMMITTED lifecycle state until it reaches `wanted` (the
/// budget-suite's determinism rule: never a wall-clock sleep against a side
/// file).
fn wait_for_state(engine: &Engine, name: &str, wanted: LifecycleState) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let entry = engine
            .blocking()
            .fleet_entry(name)
            .expect("fleet entry readable");
        if entry.state == wanted {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{name} never reached {wanted:?} (now {:?})",
            entry.state
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn shutdown_stops_every_running_agent_gracefully_and_surfaces_each() {
    let state = tempfile::tempdir().unwrap();
    let manifest = tempfile::tempdir().unwrap();
    let engine = Engine::open(Some(state.path().to_path_buf())).expect("engine open");
    start_agent(&engine, manifest.path(), "first");
    start_agent(&engine, manifest.path(), "second");

    let (sink_handle, sink) = SharedSink::new();
    let outcomes = engine
        .blocking()
        .shutdown(Duration::from_secs(5), Some(&sink));

    assert_eq!(
        outcomes,
        vec![
            DrainOutcome {
                name: "first".to_string(),
                result: StopResult::Graceful
            },
            DrainOutcome {
                name: "second".to_string(),
                result: StopResult::Graceful
            },
        ],
        "both running agents exit within the window, in fleet order"
    );
    wait_for_state(&engine, "first", LifecycleState::Stopped);
    wait_for_state(&engine, "second", LifecycleState::Stopped);
    let text = sink_handle.text();
    assert!(
        text.contains("stopping first (graceful window") && text.contains("stopping second"),
        "each agent's ladder is announced:\n{text}"
    );
    assert!(
        text.contains("first: stopped gracefully") && text.contains("second: stopped gracefully"),
        "each outcome is surfaced:\n{text}"
    );
}

#[test]
fn shutdown_gives_a_paused_agent_the_ladder_not_the_kill_on_drop() {
    // THE item-3 pin: a paused agent under a plain embedding used to lose
    // the graceful path entirely at drop. The drain's plan includes Paused
    // rows, and the engine's stop resumes the suspension BEFORE the ladder
    // so the graceful signal can be delivered (the #327 semantics) — the
    // committed terminal state proves the graceful arm ran, not the
    // escalation.
    let state = tempfile::tempdir().unwrap();
    let manifest = tempfile::tempdir().unwrap();
    let engine = Engine::open(Some(state.path().to_path_buf())).expect("engine open");
    start_agent(&engine, manifest.path(), "frozen");
    engine.blocking().pause("frozen").expect("pause");
    wait_for_state(&engine, "frozen", LifecycleState::Paused);

    let (sink_handle, sink) = SharedSink::new();
    let outcomes = engine
        .blocking()
        .shutdown(Duration::from_secs(5), Some(&sink));

    assert_eq!(
        outcomes,
        vec![DrainOutcome {
            name: "frozen".to_string(),
            result: StopResult::Graceful
        }],
        "the paused agent goes through the resume-then-stop ladder gracefully:\
         {outcomes:?}"
    );
    wait_for_state(&engine, "frozen", LifecycleState::Stopped);
    assert!(
        sink_handle.text().contains("frozen: stopped gracefully"),
        "the ladder ran for the paused row:\n{}",
        sink_handle.text()
    );
}

#[test]
fn shutdown_on_a_quiescent_fleet_is_an_honest_no_op() {
    let state = tempfile::tempdir().unwrap();
    let manifest = tempfile::tempdir().unwrap();
    let engine = Engine::open(Some(state.path().to_path_buf())).expect("engine open");
    // A registered-but-never-started row: no process, not the drain's
    // business — the plan is empty and the no-op is SAID, not silent.
    hekma_conformance::test_support::ManifestFixture::new("idle", &[])
        .guaranteed_on_all_oses("pause")
        .write(manifest.path());
    engine
        .blocking()
        .register_with_adapter("idle", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .expect("register");

    let (sink_handle, sink) = SharedSink::new();
    let outcomes = engine
        .blocking()
        .shutdown(Duration::from_secs(1), Some(&sink));

    assert!(
        outcomes.is_empty(),
        "no live row → no outcomes: {outcomes:?}"
    );
    assert!(
        sink_handle
            .text()
            .contains("no running or paused agents; nothing to stop"),
        "the no-op is said, not silent:\n{}",
        sink_handle.text()
    );
}

#[test]
fn shutdown_without_a_sink_returns_outcomes_only() {
    // The None-sink embedder path: identical outcomes, zero notices (the
    // stream is an OPTION, not a side effect).
    let state = tempfile::tempdir().unwrap();
    let manifest = tempfile::tempdir().unwrap();
    let engine = Engine::open(Some(state.path().to_path_buf())).expect("engine open");
    start_agent(&engine, manifest.path(), "quiet");

    let outcomes = engine.blocking().shutdown(Duration::from_secs(5), None);

    assert_eq!(
        outcomes,
        vec![DrainOutcome {
            name: "quiet".to_string(),
            result: StopResult::Graceful
        }],
        "no sink, same ladder, same outcome"
    );
    wait_for_state(&engine, "quiet", LifecycleState::Stopped);
}
