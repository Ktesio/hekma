//! Integration tests for the declared liveness check (contract `1.1.0`): a
//! manifest's optional `[liveness]` section makes the reaper evaluate ONE
//! declared signal against the RUNNING agent, and a signal failing
//! CONTINUOUSLY for the declared `max_age_ms` moves it `running → failed`
//! (cause `unresponsive`) under the ordinary Restart Policy. Driven through
//! the engine's PUBLIC API + blocking facade only (spine AD-2/AD-13),
//! spawning the REAL `fake_agent` helper — whose `--heartbeat-file` /
//! `--heartbeat-stop-after` / `--http-health` / `--http-health-fail-after`
//! fixtures provide (and degrade) the liveness signals — so the whole
//! mechanism is genuinely exercised (not mocked).
//!
//! The matrix pinned here, per the story's acceptance criteria:
//! * an ABSENT heartbeat (never written ≡ stale) expires the window and
//!   fails the instance with the `unresponsive` cause — and the failure
//!   respects the window (never before `max_age_ms` has elapsed);
//! * a FRESH heartbeat keeps the instance running indefinitely;
//! * a transient blip (the heart stops, then the signal recovers INSIDE the
//!   window) recovers — one first-failure diagnostic, one recovery
//!   diagnostic, NO `unresponsive` transition;
//! * a PAUSED instance is NEVER probed (the mutation-proof pin: a suspended
//!   process cannot heartbeat, and probing it would fail it for being
//!   paused);
//! * the HTTP kind behaves identically (passing endpoint stays running; a
//!   silenced endpoint opens the window and expiry fails the instance) —
//!   proving the window semantics are the KIND-INDEPENDENT unifying
//!   semantic;
//! * the Restart Policy applies VERBATIM (an `on-failure` instance restarts
//!   through the unresponsive failures until the crash-loop threshold);
//! * a manifest WITHOUT the section keeps the pre-1.1.0 reaper behavior
//!   byte-identical (same transitions, same causes, several ticks);
//! * an ADOPTED instance re-derives its window fresh (the honest
//!   ≤-one-extra-window grace, pinned by a still-running assertion at the
//!   original window's expiry).

use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hekma_engine::{
    AdapterRef, DiagnosticSink, Engine, LifecycleState, RestartPolicy, TransitionCause,
};
use tempfile::TempDir;

/// Write a manifest whose `[lifecycle.start]` exec is `fake_agent` + `args`,
/// with `liveness_body` (if any) appended as the raw `[liveness]` section
/// (sub-table header included) and `pause_guaranteed` adding the pause
/// capability (the paused-pin fixture needs it; nobody else).
fn write_manifest(
    dir: &Path,
    kind: &str,
    args: &[&str],
    liveness_body: Option<&str>,
    pause_guaranteed: bool,
) {
    let fixture = hekma_conformance::test_support::ManifestFixture::new(kind, args)
        .guaranteed_on_all_oses("interaction");
    let fixture = match liveness_body {
        Some(body) => fixture.liveness(body),
        None => fixture,
    };
    let fixture = if pause_guaranteed {
        fixture.guaranteed_on_all_oses("pause")
    } else {
        fixture
    };
    fixture.write(dir);
}

fn open(base: &TempDir) -> Engine {
    Engine::open(Some(base.path().to_path_buf())).expect("open engine")
}

/// Poll `instance_status` until `pred(state)` holds, bounded.
fn wait_until_state(
    facade: &hekma_engine::Blocking<'_>,
    name: &str,
    pred: impl Fn(LifecycleState) -> bool,
    within: Duration,
    what: &str,
) -> LifecycleState {
    let deadline = Instant::now() + within;
    loop {
        let state = facade
            .instance_status(name)
            .map(|s| s.instance.state)
            .unwrap_or(LifecycleState::Registered);
        if pred(state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} (last state: {state})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Poll the captured sink until a line matching `needle` appears, bounded.
/// Returns the line (the diagnostics-once assertions read it for wording).
fn wait_for_diagnostic(
    shared: &Arc<Mutex<Vec<u8>>>,
    needle: &str,
    within: Duration,
    what: &str,
) -> String {
    let deadline = Instant::now() + within;
    loop {
        let captured = String::from_utf8(shared.lock().unwrap().clone()).expect("utf-8 sink");
        if let Some(line) = captured.lines().find(|l| l.contains(needle)) {
            return line.to_string();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} (sink so far:\n{captured})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Count the sink lines matching `needle` right now (the diagnostics-once
/// pins assert the count is EXACTLY one).
fn diagnostic_count(shared: &Arc<Mutex<Vec<u8>>>, needle: &str) -> usize {
    String::from_utf8(shared.lock().unwrap().clone())
        .expect("utf-8 sink")
        .lines()
        .filter(|l| l.contains(needle))
        .count()
}

/// Grab a free loopback port by binding port 0 and immediately dropping the
/// listener (the standard test pattern; the fake_agent re-binds it at spawn).
fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

#[test]
fn absent_heartbeat_expires_the_window_and_fails_unresponsive() {
    // The core semantic: a declared heartbeat that NEVER beats (absent ≡
    // stale) opens the failure window on the reaper's first tick and fails
    // the instance when the window expires — with the `unresponsive` cause
    // naming the check and its failing streak. The window is ENFORCED, not
    // observed: the failure cannot land before `max_age_ms` has actually
    // elapsed since the instance went running.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let max_age_ms = 600u64;
    write_manifest(
        manifest.path(),
        "silent-hb",
        &[
            "--heartbeat-file",
            "hb.marker:100",
            "--heartbeat-stop-after",
            "0",
            "--linger-ms",
            "600000",
        ],
        Some(&format!(
            "[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = {max_age_ms}"
        )),
        false,
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(&shared)))));
    let engine =
        Engine::open_with_diagnostics(Some(state.path().to_path_buf()), sink).expect("open");
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "silent-hb",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    // Pin the TERMINAL path: `never` (not the AD-15 `on-failure` default, which
    // would restart into a crash loop here). The policy conclusion — "not
    // restarting" — rides the reaper's ONE terminal diagnostic (the frozen
    // `unresponsive` cause shape carries no detail field), so the sink
    // captures it.
    facade
        .set_restart_policy("silent-hb", RestartPolicy::Never)
        .unwrap();

    let started = facade.start("silent-hb").unwrap();
    assert_eq!(started.state, LifecycleState::Running);
    let began = Instant::now();

    let final_state = wait_until_state(
        &facade,
        "silent-hb",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(10),
        "the unresponsive expiry to land failed",
    );
    assert_eq!(final_state, LifecycleState::Failed);
    let elapsed = began.elapsed();
    // The window genuinely elapsed: the failure never lands before the
    // declared max_age (the deadline pin — a first-failure-equals-failure
    // mutation would land at ~0ms).
    assert!(
        elapsed >= Duration::from_millis(max_age_ms),
        "the expiry must respect the declared window (failed after {elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the expiry is prompt, not a restart loop (took {elapsed:?})"
    );

    let events = facade.transition_events("silent-hb").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.prior_state, LifecycleState::Running);
    assert_eq!(last.new_state, LifecycleState::Failed);
    match &last.cause {
        TransitionCause::Unresponsive {
            check,
            failing_for_ms,
        } => {
            assert!(
                check.contains("heartbeat-file") && check.contains("hb.marker"),
                "the cause names the check: {check}"
            );
            assert!(
                *failing_for_ms >= max_age_ms,
                "the failing streak reached the window: {failing_for_ms}ms"
            );
        }
        other => panic!("expected an unresponsive cause, got {other:?}"),
    }

    // Default Restart Policy is `never` here (pinned above): the instance
    // stays failed, NO restart ran, and the terminal conclusion — the policy
    // is why nothing follows — reached the reaper's surface exactly once.
    let status = facade.instance_status("silent-hb").unwrap();
    assert_eq!(
        status.restart_count, 0,
        "no restart under the pinned policy"
    );
    assert_eq!(
        diagnostic_count(&shared, "restart policy is 'never'"),
        1,
        "exactly ONE terminal conclusion diagnostic (sink: captured)"
    );
}

#[test]
fn fresh_heartbeat_keeps_the_instance_running() {
    // The healthy path: a heartbeat beating every 100ms against a 5s window
    // stays running well past several windows — no failure, no window
    // diagnostic (a fresh signal is quiet by design).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "beating-hb",
        &["--heartbeat-file", "hb.marker:100", "--linger-ms", "600000"],
        Some("[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = 5000"),
        false,
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(&shared)))));
    let engine =
        Engine::open_with_diagnostics(Some(state.path().to_path_buf()), sink).expect("open");
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "beating-hb",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let started = facade.start("beating-hb").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // Several windows' worth of ticks (the reaper evaluates every ~250ms):
    // still running, and the reaper surface stayed QUIET (no window opened,
    // so there is nothing to say — surfacing is for edges, not steady state).
    std::thread::sleep(Duration::from_millis(2500));
    let status = facade.instance_status("beating-hb").unwrap();
    assert_eq!(status.instance.state, LifecycleState::Running);
    assert_eq!(status.restart_count, 0);
    let captured = String::from_utf8(shared.lock().unwrap().clone()).expect("utf-8 sink");
    assert!(
        !captured.contains("declared liveness check"),
        "a fresh heartbeat must not open a window (sink: {captured})"
    );

    facade
        .stop("beating-hb", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn heartbeat_blip_recovers_inside_the_window_without_failing() {
    // The blip-recovery pin: the heart STOPS at ~900ms, the window (1500ms)
    // opens on the first stale read — and the signal RECOVERS (the test
    // touches the heartbeat file directly, race-free) before expiry. The
    // instance never fails; exactly ONE first-failure diagnostic and ONE
    // recovery diagnostic are emitted (the edges an operator wants, once
    // each), and no `unresponsive` transition ever fires.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "blip-hb",
        &[
            "--heartbeat-file",
            "hb.marker:150",
            "--heartbeat-stop-after",
            "900",
            "--linger-ms",
            "600000",
        ],
        Some("[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = 1500"),
        false,
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(&shared)))));
    let engine =
        Engine::open_with_diagnostics(Some(state.path().to_path_buf()), sink).expect("open");
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "blip-hb",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let started = facade.start("blip-hb").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // The heart stopped at ~900ms; the reaper's next tick opens the window.
    let first_failure = wait_for_diagnostic(
        &shared,
        "declared liveness check failing",
        Duration::from_secs(6),
        "the first-failure diagnostic",
    );
    assert!(
        first_failure.contains("blip-hb") && first_failure.contains("hb.marker"),
        "the first-failure note names the instance + check: {first_failure}"
    );

    // RECOVER: touch the heartbeat file directly in the Agent Home (the
    // engine only reads the mtime — an external writer is indistinguishable
    // from the agent beating again, which is exactly the declared semantic).
    let status = facade.instance_status("blip-hb").unwrap();
    let home = Path::new(&status.instance.agent_home);
    std::fs::write(home.join("hb.marker"), b"recovered").expect("touch the heartbeat");

    let recovered = wait_for_diagnostic(
        &shared,
        "declared liveness check recovered",
        Duration::from_secs(6),
        "the recovery diagnostic",
    );
    assert!(
        recovered.contains("blip-hb"),
        "the recovery note names the instance: {recovered}"
    );

    // The agent "keeps beating" after its recovery (the test plays the
    // healthy agent from here on — touching every 400ms against the 1500ms
    // window), so the recovered state is STABLE, not a second blip in the
    // making.
    for _ in 0..3 {
        std::fs::write(home.join("hb.marker"), b"beat").expect("re-touch the heartbeat");
        std::thread::sleep(Duration::from_millis(400));
    }

    // Well past where an UNRECOVERED window would have expired (first
    // failure + 1500ms): still running, exactly ONE of each diagnostic, and
    // no unresponsive transition ever fired.
    std::thread::sleep(Duration::from_millis(500));
    let status = facade.instance_status("blip-hb").unwrap();
    assert_eq!(status.instance.state, LifecycleState::Running);
    assert_eq!(
        diagnostic_count(&shared, "declared liveness check failing"),
        1,
        "exactly ONE first-failure diagnostic"
    );
    assert_eq!(
        diagnostic_count(&shared, "declared liveness check recovered"),
        1,
        "exactly ONE recovery diagnostic"
    );
    let events = facade.transition_events("blip-hb").unwrap();
    assert!(
        events
            .iter()
            .all(|e| !matches!(e.cause, TransitionCause::Unresponsive { .. })),
        "a recovered blip never fails the instance: {events:#?}"
    );

    facade
        .stop("blip-hb", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn paused_instance_is_never_probed_even_with_a_stale_signal() {
    // THE PAUSED PIN (mutation-proof): a PAUSED instance is never probed —
    // a suspended process cannot heartbeat, and probing it would fail it
    // for being paused. The heartbeat is absent from the very start
    // (`--heartbeat-stop-after 0`), the window (500ms) is LONG past, and
    // after several seconds of reaper ticks the instance is STILL `paused`.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "frozen-hb",
        &[
            "--heartbeat-file",
            "hb.marker:100",
            "--heartbeat-stop-after",
            "0",
            "--linger-ms",
            "600000",
        ],
        Some("[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = 500"),
        true,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "frozen-hb",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let started = facade.start("frozen-hb").unwrap();
    assert_eq!(started.state, LifecycleState::Running);
    let paused = facade.pause("frozen-hb").unwrap();
    assert_eq!(paused.state, LifecycleState::Paused);

    // Several windows' worth of ticks while suspended: the paused pin holds.
    std::thread::sleep(Duration::from_millis(2500));
    let status = facade.instance_status("frozen-hb").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Paused,
        "a paused instance is never probed — a suspension must never fail the agent"
    );
    assert_eq!(status.restart_count, 0);

    // And resuming re-arms the check: the (still absent) heartbeat now
    // expires the window normally — the guard is the PAUSE, not a blanket
    // amnesty for the instance.
    facade.resume("frozen-hb").unwrap();
    let final_state = wait_until_state(
        &facade,
        "frozen-hb",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(10),
        "the resumed instance's window to expire",
    );
    assert_eq!(final_state, LifecycleState::Failed);
}

#[test]
fn http_health_passing_then_silenced_runs_out_its_window_before_failing() {
    // The HTTP kind under the SAME window semantic — two subjects in one
    // drill: while the endpoint answers, the instance runs; after
    // `--http-health-fail-after 600` silences it, the failure WINDOW opens
    // (first-failure diagnostic) but a 5s window means the instance is
    // STILL running well past the onset — the pin that a failing signal
    // alone fails nothing (the window is the semantic). Expiry is covered
    // by the sibling test below.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let port = free_port();
    write_manifest(
        manifest.path(),
        "quiet-http",
        &[
            "--http-health",
            &format!("{port}:200"),
            "--http-health-fail-after",
            "600",
            "--linger-ms",
            "600000",
        ],
        Some(&format!(
            "[liveness.http_get]\nurl = \"http://127.0.0.1:{port}/health\"\nexpected_status = 200\nmax_age_ms = 5000"
        )),
        false,
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(&shared)))));
    let engine =
        Engine::open_with_diagnostics(Some(state.path().to_path_buf()), sink).expect("open");
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "quiet-http",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let started = facade.start("quiet-http").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // The endpoint went silent at ~600ms; the window opens on the next tick.
    wait_for_diagnostic(
        &shared,
        "declared liveness check failing",
        Duration::from_secs(6),
        "the first-failure diagnostic after the silence",
    );

    // Well past the onset, still inside the 5s window: STILL RUNNING — the
    // fail-window pin (a first-failure-equals-failure mutation lands this
    // failed at ~600ms).
    std::thread::sleep(Duration::from_millis(2000));
    let status = facade.instance_status("quiet-http").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Running,
        "a silenced endpoint inside its window must not fail the instance"
    );
    assert_eq!(
        diagnostic_count(&shared, "declared liveness check failing"),
        1,
        "exactly ONE first-failure diagnostic (no spam while the window stays open)"
    );

    facade
        .stop("quiet-http", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn http_health_silence_expires_the_window_and_fails_unresponsive() {
    // The HTTP expiry: the endpoint silences at ~600ms, the 700ms window
    // expires ~1300ms in, and the instance lands `failed` with the
    // `unresponsive` cause naming the HTTP check — the kind-independence of
    // the unifying semantic, pinned end to end.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let port = free_port();
    let max_age_ms = 700u64;
    write_manifest(
        manifest.path(),
        "dead-http",
        &[
            "--http-health",
            &format!("{port}:200"),
            "--http-health-fail-after",
            "600",
            "--linger-ms",
            "600000",
        ],
        Some(&format!(
            "[liveness.http_get]\nurl = \"http://127.0.0.1:{port}/health\"\nexpected_status = 200\nmax_age_ms = {max_age_ms}"
        )),
        false,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "dead-http",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let started = facade.start("dead-http").unwrap();
    assert_eq!(started.state, LifecycleState::Running);
    let began = Instant::now();

    let final_state = wait_until_state(
        &facade,
        "dead-http",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(10),
        "the HTTP window to expire",
    );
    assert_eq!(final_state, LifecycleState::Failed);
    let elapsed = began.elapsed();
    // The silence began ~600ms in; the expiry cannot precede onset + window.
    assert!(
        elapsed >= Duration::from_millis(600 + max_age_ms - 150),
        "the HTTP expiry respects onset + window (failed after {elapsed:?})"
    );

    let events = facade.transition_events("dead-http").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.prior_state, LifecycleState::Running);
    assert_eq!(last.new_state, LifecycleState::Failed);
    match &last.cause {
        TransitionCause::Unresponsive { check, .. } => {
            assert!(
                check.contains("http-get") && check.contains(&port.to_string()),
                "the cause names the HTTP check: {check}"
            );
        }
        other => panic!("expected an unresponsive cause, got {other:?}"),
    }
}

#[test]
fn unresponsive_failures_restart_under_on_failure_until_the_crash_loop() {
    // The Restart Policy applies VERBATIM: an `on-failure` instance whose
    // declared heartbeat never beats restarts through its unresponsive
    // failures exactly like crash failures — the production backoff paces
    // the attempts (1s, 2s, 4s, 8s), and at the 5-consecutive threshold the
    // crash-loop conclusion lands TERMINAL (accepted-precedent runtime:
    // every Default-Engine restart drill pays it). Each restart gets a
    // FRESH window (the Run resets the streak) — the drill proves the whole
    // chain: expire → restart → expire → … → crash-loop.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "loop-hb",
        &[
            "--heartbeat-file",
            "hb.marker:100",
            "--heartbeat-stop-after",
            "0",
            "--linger-ms",
            "600000",
        ],
        Some("[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = 400"),
        false,
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(&shared)))));
    let engine =
        Engine::open_with_diagnostics(Some(state.path().to_path_buf()), sink).expect("open");
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "loop-hb",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("loop-hb", RestartPolicy::OnFailure)
        .unwrap();

    let started = facade.start("loop-hb").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // First expiry: the unresponsive edge, then the restart (fresh window).
    wait_until_state(
        &facade,
        "loop-hb",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(10),
        "the first unresponsive expiry",
    );
    let events = facade.transition_events("loop-hb").unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.cause, TransitionCause::Unresponsive { .. })),
        "the failures are unresponsive causes: {events:#?}"
    );
    wait_until_state(
        &facade,
        "loop-hb",
        |s| s == LifecycleState::Running,
        Duration::from_secs(20),
        "the first policy restart to land running (fresh window)",
    );
    let status = facade.instance_status("loop-hb").unwrap();
    assert!(
        status.restart_count >= 1,
        "the Restart Policy restarted through the unresponsive failure"
    );

    // The remaining cycles ride the production backoff to the crash loop:
    // eventually the terminal conclusion — the frozen `unresponsive` cause
    // carries no detail field, so the crash-loop verdict reaches the surface
    // as the reaper's ONE terminal diagnostic.
    wait_for_diagnostic(
        &shared,
        "crash-loop: 5 consecutive failures",
        Duration::from_secs(60),
        "the crash-loop conclusion diagnostic",
    );
    let status = facade.instance_status("loop-hb").unwrap();
    assert_eq!(status.instance.state, LifecycleState::Failed);
    // The terminal settle (F-Low-2) DELETES the record — the live count dies
    // with it (the honest state: nothing will restart). The count the drill
    // REACHED was asserted mid-run above; the crash-loop conclusion itself
    // lives in the event log + the ONE diagnostic.
    assert_eq!(
        status.restart_count, 0,
        "the terminal settle cleared the record"
    );
    // The verdict is FINAL: no further restart follows the terminal
    // conclusion.
    std::thread::sleep(Duration::from_millis(1500));
    let status = facade.instance_status("loop-hb").unwrap();
    assert_eq!(status.instance.state, LifecycleState::Failed);
    assert_eq!(status.restart_count, 0, "no restart follows the crash loop");
}

#[test]
fn no_liveness_section_reaper_pass_is_byte_identical() {
    // The byte-identity PIN, reaper-side: a manifest WITHOUT `[liveness]`
    // keeps the exact pre-1.1.0 event sequence across several full reaper
    // passes — the same two transitions with the same causes, no unresponsive
    // edge, no added noise. The absent-section behavior is BYTE-identical.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "plain",
        &["--linger-ms", "600000"],
        None,
        false,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "plain",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let started = facade.start("plain").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // Several full reaper passes (~1.5s ≈ 6 ticks) before reading events.
    std::thread::sleep(Duration::from_millis(1500));

    let events = facade.transition_events("plain").unwrap();
    let sequence: Vec<(LifecycleState, LifecycleState, &TransitionCause)> = events
        .iter()
        .map(|e| (e.prior_state, e.new_state, &e.cause))
        .collect();
    assert_eq!(
        sequence,
        vec![
            (
                LifecycleState::Registered,
                LifecycleState::Starting,
                &TransitionCause::Command {
                    command: "start".to_string()
                }
            ),
            (
                LifecycleState::Starting,
                LifecycleState::Running,
                &TransitionCause::AdapterReady
            ),
        ],
        "the no-section reaper passes are exactly the pre-1.1.0 sequence: {events:#?}"
    );

    facade.stop("plain", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn adopted_instance_rederives_a_fresh_window() {
    // The adoption pin: the failure window lives in SUPERVISION, not in the
    // record — an adopted instance RE-DERIVES it fresh (the honest
    // ≤-one-extra-window grace). The agent is DETACHED-started (its handle
    // survives engine A's drop), its heartbeat is absent, the window is
    // 2500ms. Engine A closes at ~1s — BEFORE the original window expires.
    // Engine B adopts: the instance is STILL RUNNING at (original start +
    // 2500ms) — a persisted/continued window would have failed it — and
    // fails only after B's OWN window (adoption + 2500ms) expires.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "adopted-hb",
        &[
            "--heartbeat-file",
            "hb.marker:100",
            "--heartbeat-stop-after",
            "0",
            "--linger-ms",
            "600000",
        ],
        Some("[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = 2500"),
        false,
    );

    let began = Instant::now();
    {
        let engine_a = open(&state);
        let facade_a = engine_a.blocking();
        facade_a
            .register_with_adapter(
                "adopted-hb",
                &AdapterRef::Manifest(manifest.path().to_path_buf()),
            )
            .unwrap();
        let started = facade_a.start_detached("adopted-hb").unwrap();
        assert_eq!(started.state, LifecycleState::Running);
        // Engine A closes at ~1s — before its own window (2500ms) expires.
        std::thread::sleep(Duration::from_millis(1000));
    } // engine A drops; the DETACHED agent survives (disarmed handles).

    let engine_b = open(&state);
    let facade_b = engine_b.blocking();
    // The fresh window's origin is the ADOPTION (engine B's open re-acquires
    // and stamps a new `Supervised`).
    let adopted_at = Instant::now();
    let status = facade_b.instance_status("adopted-hb").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Running,
        "the adopted instance is re-held running"
    );

    // (original start + ~2500ms): STILL RUNNING under engine B — the fresh
    // window pin. The original window's expiry passes while THIS engine's
    // window (opened at adoption) is still open.
    let original_expiry = began + Duration::from_millis(2500);
    let now = Instant::now();
    if now < original_expiry {
        std::thread::sleep(original_expiry - now);
    }
    let status = facade_b.instance_status("adopted-hb").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Running,
        "the adopted window is FRESH — the original window's expiry must not fail it"
    );

    // Engine B's own window (adoption + 2500ms) expires: the instance fails
    // with the unresponsive cause, and never BEFORE B's window could have
    // elapsed. (The marker started at ADOPTION — the fresh window's origin.)
    let final_state = wait_until_state(
        &facade_b,
        "adopted-hb",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(10),
        "the adopted instance's fresh window to expire",
    );
    assert_eq!(final_state, LifecycleState::Failed);
    assert!(
        adopted_at.elapsed() >= Duration::from_millis(2300),
        "the adopted expiry waited out B's own (fresh) window: {:?}",
        adopted_at.elapsed()
    );
    let events = facade_b.transition_events("adopted-hb").unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.cause, TransitionCause::Unresponsive { .. })),
        "the adopted expiry is the same unresponsive edge: {events:#?}"
    );
}

/// A `Write` adapter delegating into a shared buffer (the
/// `diagnostic_sink.rs` shape): the test reads the sink's captured bytes
/// while the engine owns the boxed writer.
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
