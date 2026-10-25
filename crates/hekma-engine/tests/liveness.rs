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
//! * a FLAPPING signal (each blip recovering inside the window) is bounded:
//!   three open/recover diagnostic pairs, ONE flap note, silence after —
//!   while the window itself keeps never expiring (review round 1, P6);
//! * an expired DETACHED wedge is force-killed before the failure commits,
//!   and the `on-failure` restart re-binds the freed port (review round 1,
//!   P4);
//! * the expiry verdict honestly names pre-existing staleness ("already ~Nms
//!   stale at first observation") in BOTH the terminal diagnostic and the
//!   cause's optional `detail` (review round 1, P7/P8);
//! * an ADOPTED instance whose adapter snapshot or manifest became unreadable
//!   degrades to PROCESS-liveness supervision with ONE surfaced diagnostic —
//!   never a failed adoption (review round 1, V3);
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
    // restarting" — rides the reaper's ONE terminal diagnostic (the
    // `unresponsive` cause's verdict detail, when present, rides the cause
    // itself and this diagnostic), so the sink captures it.
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
    // And it is PROMPT: one tick to observe the onset, one to detect the
    // expiry, plus scheduler slop — not a blanket ceiling (V1's upper bound;
    // a 5s blanket could hide a window running 4x slow).
    assert!(
        elapsed < Duration::from_millis(max_age_ms + 500 + 750),
        "the expiry lands within the window plus two ticks of slop (failed after {elapsed:?})"
    );

    // The first-failure diagnostic announced the DECLARED window verbatim —
    // the operator plans against what was announced (V1).
    let captured = String::from_utf8(shared.lock().unwrap().clone()).expect("utf-8 sink");
    let first_failure = captured
        .lines()
        .find(|l| l.contains("declared liveness check failing"))
        .expect("the first-failure diagnostic announced the window");
    let announced: u64 = first_failure
        .split("failing continuously for ")
        .nth(1)
        .and_then(|rest| rest.split("ms fails the instance").next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("the announcement carries the window in ms: {first_failure}"));
    assert_eq!(
        announced, max_age_ms,
        "the announcement names the DECLARED window: {first_failure}"
    );

    let events = facade.transition_events("silent-hb").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.prior_state, LifecycleState::Running);
    assert_eq!(last.new_state, LifecycleState::Failed);
    match &last.cause {
        TransitionCause::Unresponsive {
            check,
            failing_for_ms,
            detail,
        } => {
            assert!(
                check.contains("heartbeat-file") && check.contains("hb.marker"),
                "the cause names the check: {check}"
            );
            assert!(
                *failing_for_ms >= max_age_ms,
                "the failing streak reached the window: {failing_for_ms}ms"
            );
            assert!(
                *failing_for_ms < max_age_ms + 1000,
                "the streak is the observed window plus tick slop, not a runaway: \
                 {failing_for_ms}ms"
            );
            // A plain silence expiry (no pre-existing staleness) carries no
            // ONSET note — but under the `never` policy this expiry is
            // TERMINAL, so the policy verdict rides the cause's detail (P8):
            // the event log carries the "will never restart" conclusion, not
            // just the reaper's one console diagnostic.
            assert_eq!(
                detail.as_deref(),
                Some("restart policy is 'never' — not restarting"),
                "the terminal policy verdict rides the cause: {detail:?}"
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
    // for being paused. The RESUME side is pinned just as hard (review
    // round 1, P1): resuming CLEARS the open failure window, so the check
    // re-arms from ZERO — a window that was already partly elapsed before
    // the pause may not survive the suspension and insta-fail the resumed
    // agent. The heartbeat is absent from the very start
    // (`--heartbeat-stop-after 0`), so the window is genuinely OPEN before
    // the pause (the diagnostic below proves supervision saw it) — the
    // carry-over mutation has a real window to steal.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let max_age_ms = 1000u64;
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
        Some(&format!(
            "[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = {max_age_ms}"
        )),
        true,
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(&shared)))));
    let engine =
        Engine::open_with_diagnostics(Some(state.path().to_path_buf()), sink).expect("open");
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "frozen-hb",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let started = facade.start("frozen-hb").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // The first reaper tick opens the window (the absent heartbeat is seen
    // stale) — the pre-pause staleness the resume must NOT inherit.
    wait_for_diagnostic(
        &shared,
        "declared liveness check failing",
        Duration::from_secs(6),
        "the pre-pause first-failure diagnostic",
    );
    // Part of the window elapses while running...
    std::thread::sleep(Duration::from_millis(300));
    let paused = facade.pause("frozen-hb").unwrap();
    assert_eq!(paused.state, LifecycleState::Paused);

    // More than a full window's worth of ticks while suspended: the paused
    // pin holds, and a surviving window would be LONG past expiry.
    std::thread::sleep(Duration::from_millis(1500));
    let status = facade.instance_status("frozen-hb").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Paused,
        "a paused instance is never probed — a suspension must never fail the agent"
    );
    assert_eq!(status.restart_count, 0);

    // Resuming re-arms the check from zero (P1): the failure cannot land
    // until a FULL window has elapsed AFTER the resume. (The carry-over
    // mutation — resume keeps the pre-pause `failing_since` — insta-fails
    // within one tick of the resume, far under this bound. The guard is the
    // PAUSE, not a blanket amnesty: the still-absent heartbeat still fails
    // the instance, on the resumed window's schedule.)
    facade.resume("frozen-hb").unwrap();
    let resumed_at = Instant::now();
    let final_state = wait_until_state(
        &facade,
        "frozen-hb",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(10),
        "the resumed instance's FRESH window to expire",
    );
    assert_eq!(final_state, LifecycleState::Failed);
    let after_resume = resumed_at.elapsed();
    assert!(
        after_resume >= Duration::from_millis(max_age_ms),
        "the resume re-armed the window from zero (failed {after_resume:?} after the \
         resume; the declared window is {max_age_ms}ms)"
    );
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
    // The silence began ~600ms in; the expiry cannot precede onset + window,
    // and it is PROMPT: the window plus two ticks of slop (V1's upper bound).
    assert!(
        elapsed >= Duration::from_millis(600 + max_age_ms - 150),
        "the HTTP expiry respects onset + window (failed after {elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_millis(600 + max_age_ms + 1250),
        "the HTTP expiry is prompt, not a stalled reaper (failed after {elapsed:?})"
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
    // eventually the terminal conclusion — the crash-loop verdict folds
    // into the PERSISTED cause's `detail` (so the event log and `show`'s
    // failed-cause fold carry it), and the reaper's ONE diagnostic remains
    // the immediate-console surface.
    wait_for_diagnostic(
        &shared,
        "crash-loop: 5 consecutive failures",
        Duration::from_secs(60),
        "the crash-loop conclusion diagnostic",
    );
    // P8: the terminal verdict rides the PERSISTED cause. The terminal
    // `running → failed` transition's `Unresponsive` cause carries the
    // crash-loop conclusion in its `detail` — the event log is the durable
    // surface, byte-distinguishable from a mid-loop failure.
    let events = facade.transition_events("loop-hb").unwrap();
    let terminal = events
        .iter()
        .rev()
        .find(|e| e.prior_state == LifecycleState::Running && e.new_state == LifecycleState::Failed)
        .expect("a terminal running → failed transition event");
    match &terminal.cause {
        TransitionCause::Unresponsive { detail, .. } => {
            let detail = detail.as_deref().unwrap_or_default();
            assert!(
                detail.contains("crash-loop: 5 consecutive failures"),
                "the terminal cause's detail carries the crash-loop verdict: {detail:?}"
            );
        }
        other => panic!("expected an Unresponsive terminal cause, got {other:?}"),
    }
    let status = facade.instance_status("loop-hb").unwrap();
    assert_eq!(status.instance.state, LifecycleState::Failed);
    // The `show` surface is pinned too: `failed_cause` folds the last
    // failure event's cause (the terminal settle cleared the record's own
    // cause), so the verdict must survive the fold.
    assert!(
        status
            .failed_cause
            .as_deref()
            .is_some_and(|cause| cause.contains("crash-loop: 5 consecutive failures")),
        "`show`'s failed-cause fold carries the verdict: {:?}",
        status.failed_cause
    );
    // The terminal settle (F-Low-2) DELETES the record — the live count dies
    // with it (the honest state: nothing will restart). The count the drill
    // REACHED was asserted mid-run above; the crash-loop conclusion lives in
    // the persisted cause (asserted above: event log + `show`'s fold) plus
    // the ONE immediate-console diagnostic.
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

    // (V4) The clock starts BEFORE the open: the fresh window's origin is the
    // ADOPTION (engine B's open re-acquires and stamps a new `Supervised`),
    // and the open itself takes time — the bound below credits that honestly
    // instead of the old fudge (`>= 2300` hardcoded), which would let a
    // 200ms-shorter window pass.
    let open_started = Instant::now();
    let engine_b = open(&state);
    let facade_b = engine_b.blocking();
    let adopted_at = Instant::now();
    let adoption_epsilon = adopted_at.saturating_duration_since(open_started);
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
    // The adopted expiry waited out B's OWN (fresh) 2500ms window: the
    // earliest honest bound is (window − the open time already spent before
    // `adopted_at` was stamped − one tick of slop), saturating at zero (V4).
    assert!(
        adopted_at.elapsed()
            >= Duration::from_millis(2500)
                .saturating_sub(adoption_epsilon)
                .saturating_sub(Duration::from_millis(250)),
        "the adopted expiry waited out B's own (fresh) window: {:?} (open took \
         {adoption_epsilon:?})",
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

#[test]
fn flapping_signal_bounds_its_diagnostic_pairs_and_notes_the_flap() {
    // Review round 1 (P6): a signal that keeps recovering INSIDE its window
    // never expires — contract-correct (persistence is the window's job) —
    // but an open/recover diagnostic PAIR per flap is per-tick spam in slow
    // motion. The pair is BOUNDED: the first three cycles emit normally, the
    // third recovery adds ONE flap note, and every later cycle is SILENT
    // while the window keeps working. The subject beats every 400ms against
    // a 300ms window: each inter-beat gap is stale for ~100ms, so the
    // ~250ms reaper tick alternately sees stale then fresh — a continuous
    // flap cadence the drill rides (and a ~100ms stale band can never hold
    // the consecutive stale ticks a 300ms expiry would need).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "flutter-hb",
        &["--heartbeat-file", "hb.marker:400", "--linger-ms", "600000"],
        Some("[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = 300"),
        false,
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(&shared)))));
    let engine =
        Engine::open_with_diagnostics(Some(state.path().to_path_buf()), sink).expect("open");
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "flutter-hb",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    // Terminal policy: if the window ever DID expire (a cadence pathology),
    // the instance must stay visibly failed — the Running pins below would
    // catch it — instead of silently restarting into a new Run's budget.
    facade
        .set_restart_policy("flutter-hb", RestartPolicy::Never)
        .unwrap();

    let started = facade.start("flutter-hb").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // Ride the flaps until the budget note lands (roughly one flap per
    // ~500ms of wall clock, plus generous scheduler slop).
    let flap_note = wait_for_diagnostic(
        &shared,
        "further liveness-window reopen diagnostics suppressed",
        Duration::from_secs(20),
        "the ONE flap-budget note",
    );
    assert!(
        flap_note.contains("flutter-hb"),
        "the note names the instance: {flap_note}"
    );
    // At note time the budget is EXACTLY spent: three announced opens, three
    // announced recoveries, ONE note.
    assert_eq!(
        diagnostic_count(&shared, "declared liveness check failing"),
        3,
        "the three in-budget cycles each announced their window"
    );
    assert_eq!(
        diagnostic_count(&shared, "declared liveness check recovered"),
        3,
        "the three in-budget cycles each announced their recovery"
    );
    assert_eq!(
        diagnostic_count(
            &shared,
            "further liveness-window reopen diagnostics suppressed"
        ),
        1,
        "exactly ONE flap note"
    );

    // Several MORE flaps' worth of ticks: silence — the budget holds — while
    // the window itself keeps never expiring (the instance is still running).
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(
        diagnostic_count(&shared, "declared liveness check failing"),
        3,
        "later flaps are silent — the budget holds"
    );
    assert_eq!(
        diagnostic_count(&shared, "declared liveness check recovered"),
        3,
        "later recoveries are silent — the budget holds"
    );
    assert_eq!(
        diagnostic_count(
            &shared,
            "further liveness-window reopen diagnostics suppressed"
        ),
        1,
        "the flap note is once-ever, never re-emitted"
    );
    let status = facade.instance_status("flutter-hb").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Running,
        "a flapping-but-recovering signal never expires the window"
    );

    facade
        .stop("flutter-hb", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn expiry_verdict_names_pre_existing_staleness_in_diagnostic_and_cause() {
    // Review round 1 (P7/P8): `failing_for_ms` counts from the first
    // OBSERVED failure — but for a heartbeat file the engine CAN say how
    // much staler the file already was at that observation. The drill
    // backdates a heartbeat file ~5s (the never-beating agent leaves it
    // absent; the test plants it), lets the 600ms window expire, and pins
    // the onset note in BOTH surfaces: the terminal diagnostic AND the
    // cause's optional `detail` — the verdict survives everywhere the cause
    // does (`show`, the transition line), not only in one diagnostic.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let max_age_ms = 600u64;
    write_manifest(
        manifest.path(),
        "ancient-hb",
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
            "ancient-hb",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("ancient-hb", RestartPolicy::Never)
        .unwrap();

    let started = facade.start("ancient-hb").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // Plant the heartbeat file OURSELVES with an mtime backdated ~5s: at the
    // reaper's first observation the file is already ~5s stale — staleness
    // the 600ms observed window cannot see and must now confess.
    let status = facade.instance_status("ancient-hb").unwrap();
    let stale_marker = Path::new(&status.instance.agent_home).join("hb.marker");
    let planted = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&stale_marker)
        .expect("plant the pre-staled heartbeat file");
    let backdated = std::time::SystemTime::now()
        .checked_sub(Duration::from_secs(5))
        .expect("a now minus 5s");
    planted
        .set_times(std::fs::FileTimes::new().set_modified(backdated))
        .expect("backdate the heartbeat mtime");
    drop(planted);

    wait_until_state(
        &facade,
        "ancient-hb",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(10),
        "the pre-staled window to expire",
    );

    // The terminal diagnostic carries the onset note...
    let terminal = wait_for_diagnostic(
        &shared,
        "already ~",
        Duration::from_secs(5),
        "the onset note in the terminal diagnostic",
    );
    assert!(
        terminal.contains("ms stale at first observation"),
        "the note names the first observation: {terminal}"
    );
    // ...and names ~5s of PRE-EXISTING staleness — far more than the 600ms
    // the window itself observed (a fabricated 0ms or a window-sized number
    // fails this bound).
    let stale_delta_ms: u64 = terminal
        .split("already ~")
        .nth(1)
        .and_then(|rest| rest.split("ms stale").next())
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("the note carries the stale delta in ms: {terminal}"));
    assert!(
        (4000..=7000).contains(&stale_delta_ms),
        "the note names ~5s of pre-existing staleness, got {stale_delta_ms}ms: {terminal}"
    );

    // The onset note rides the cause's optional detail (bounded in the
    // diagnostic by "; " on both sides) — and, P8, the TERMINAL verdict
    // folds in AFTER it: under `never` this first expiry is already
    // terminal, so the cause carries both the onset honesty and the
    // not-restarting conclusion.
    let note_start = terminal
        .find("the heartbeat file was already ~")
        .expect("the note text in the diagnostic");
    let note_end = note_start
        + terminal[note_start..]
            .find("ms stale at first observation")
            .expect("the note end")
        + "ms stale at first observation".len();
    let note = &terminal[note_start..note_end];
    let events = facade.transition_events("ancient-hb").unwrap();
    let last = events.last().unwrap();
    match &last.cause {
        TransitionCause::Unresponsive { detail, .. } => {
            let detail = detail.as_deref().expect("the cause carries expiry detail");
            assert!(
                detail.starts_with(note),
                "the onset note leads the cause detail: {detail:?}"
            );
            assert!(
                detail.contains("restart policy is 'never' — not restarting"),
                "the terminal verdict rides the cause after the onset note: {detail:?}"
            );
        }
        other => panic!("expected an unresponsive cause, got {other:?}"),
    }
}

#[test]
fn expired_detached_wedge_is_force_killed_and_the_restart_rebinds_its_port() {
    // Review round 1 (P4): a DETACHED handle has NO kill-on-drop — an
    // unresponsive expiry that merely dropped it would leave the wedged
    // process RUNNING (holding its port) while the policy restarted a
    // replacement that could never bind. The drill: an http-get instance
    // whose endpoint is silent from birth (`--http-health-fail-after 0`)
    // while the PROCESS stays alive (the silent endpoint still accepts
    // connections — a wedged, unresponsive agent). Engine A detaches and
    // drops; engine B adopts, expires the wedge, and must FORCE-KILL it:
    // at the failure the port REFUSES connections (the mutation leaves the
    // wedge accepting), and the on-failure restart then RE-BINDS the same
    // port (connect succeeds — service actually resumed).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let port = free_port();
    let max_age_ms = 1000u64;
    write_manifest(
        manifest.path(),
        "wedged-hb",
        &[
            "--http-health",
            &format!("{port}:200"),
            "--http-health-fail-after",
            "0",
            "--linger-ms",
            "600000",
        ],
        Some(&format!(
            "[liveness.http_get]\nurl = \"http://127.0.0.1:{port}/health\"\nexpected_status = 200\nmax_age_ms = {max_age_ms}"
        )),
        false,
    );

    {
        let engine_a = open(&state);
        let facade_a = engine_a.blocking();
        facade_a
            .register_with_adapter(
                "wedged-hb",
                &AdapterRef::Manifest(manifest.path().to_path_buf()),
            )
            .unwrap();
        facade_a
            .set_restart_policy("wedged-hb", RestartPolicy::OnFailure)
            .unwrap();
        let started = facade_a.start_detached("wedged-hb").unwrap();
        assert_eq!(started.state, LifecycleState::Running);
    } // engine A drops; the DETACHED wedge survives (disarmed handles).

    let engine_b = open(&state);
    let facade_b = engine_b.blocking();
    let status = facade_b.instance_status("wedged-hb").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Running,
        "the adopted wedge is re-held running"
    );

    // The endpoint has been silent since birth: B's fresh window expires it.
    wait_until_state(
        &facade_b,
        "wedged-hb",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(15),
        "the adopted wedge's window to expire",
    );

    // THE P4 PIN: at the failure the wedge is DEAD — nobody serves the port
    // as the WEDGE. The wedge's endpoint, while alive, still ACCEPTS
    // connections by design (that is the wedge), so an ESTABLISHED connect
    // is the one refuting outcome. A refused connection is the Unix shape
    // for "no listener"; a just-terminated listener can surface as a
    // different error kind on Windows, so the pin is "the connect must not
    // ESTABLISH", with the full outcome in the message when it does
    // (windows-latest triage 2026-10-24 — the bare assert could not
    // distinguish a surviving wedge from a non-refused dead-listener error
    // kind).
    // The force-kill runs BEFORE the failure commits, so Failed already
    // implies the teardown happened — and, since the backend's confirm_death
    // polls to the SIGNALED process object (layer 8), it also implies the
    // wedge is truly gone, not merely termination-initiated.
    //
    // Layer 9 (windows-latest triage 2026-10-25): that honesty flipped which
    // process can answer this connect. `Failed` now commits at TRUE death,
    // and the on-failure policy may rebind the SAME port with the
    // replacement within one supervisor cadence of that commit — on a loaded
    // runner the replacement's bind beats the test's 50ms state poll. An
    // ESTABLISHED connect at observation time is therefore ambiguous between
    // "the original wedge survived the kill" (a real supervision failure)
    // and "the replacement already rebound" (the system working as designed
    // — the bind is EXCLUSIVE, so a rebound port is itself proof the wedge
    // released it). Discriminate through the engine, not the socket: only a
    // restart that reaches Running can own an answering port; a port that
    // answers while the instance still sits Failed (no restart, or a
    // replacement that could not bind) is a genuine surviving wedge. The
    // observation window only has to span a spawn that has ALREADY BOUND —
    // the only thing that can answer this connect — so a bound replacement
    // reaches Running well inside it; a slower first bind would have left
    // this very connect refused and passed above.
    let wedge_connect =
        std::net::TcpStream::connect(std::net::SocketAddr::from(([127, 0, 0, 1], port)));
    let wedge_gone = wedge_connect.is_err();
    if !wedge_gone {
        let mut restart_owns_it = false;
        let mut last_state = LifecycleState::Failed;
        let mut last_causes: Vec<String> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(status) = facade_b.instance_status("wedged-hb") {
                last_state = status.instance.state;
                if last_state == LifecycleState::Running {
                    restart_owns_it = true;
                    break;
                }
            }
            if let Ok(events) = facade_b.transition_events("wedged-hb") {
                last_causes = events
                    .iter()
                    .rev()
                    .take(4)
                    .map(|event| format!("{:?}", event.cause))
                    .collect();
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            restart_owns_it,
            "the detached wedge was force-killed at expiry — the port must refuse \
             unless the on-failure restart already rebound it \
             (connect outcome: {wedge_connect:?}; final state: {last_state:?}; \
             recent causes: {last_causes:?})"
        );
    }

    // The on-failure restart re-binds the SAME port: wait Running, then
    // connect (bounded retries — the restart's own launch takes a beat).
    wait_until_state(
        &facade_b,
        "wedged-hb",
        |s| s == LifecycleState::Running,
        Duration::from_secs(20),
        "the policy restart to land running on the freed port",
    );
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut rebound = false;
    for _ in 0..40 {
        match std::net::TcpStream::connect(addr) {
            // The silent endpoint accepts then drops without answering, so a
            // success is connect-only (never a read).
            Ok(_) => {
                rebound = true;
                break;
            }
            // A refusal during the re-bind beat is retried (bounded).
            Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => panic!("unexpected connect error probing the re-bound port: {err}"),
        }
    }
    assert!(rebound, "the restart re-bound the freed port");

    facade_b
        .stop("wedged-hb", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn adopted_instance_with_a_corrupt_adapter_snapshot_supervises_process_liveness_only() {
    // Review round 1 (V3), outer arm: an adopted instance whose adapter
    // SNAPSHOT became unreadable cannot re-derive its declared liveness
    // check — the degradation is SURFACED as ONE diagnostic and the adoption
    // still succeeds (the agent is running and owned either way): the
    // instance is re-held with PROCESS liveness only and, with no declared
    // check supervised, stays Running past where its original window would
    // have expired. Never a failed adoption.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "hollow-hb",
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
    let home: String = {
        let engine_a = open(&state);
        let facade_a = engine_a.blocking();
        facade_a
            .register_with_adapter(
                "hollow-hb",
                &AdapterRef::Manifest(manifest.path().to_path_buf()),
            )
            .unwrap();
        let started = facade_a.start_detached("hollow-hb").unwrap();
        assert_eq!(started.state, LifecycleState::Running);
        facade_a
            .instance_status("hollow-hb")
            .unwrap()
            .instance
            .agent_home
    }; // engine A drops; the DETACHED agent survives.

    // Corrupt the snapshot AFTER A is gone (the adoption is the first reader).
    std::fs::write(Path::new(&home).join("adapter.json"), b"this is not json")
        .expect("corrupt the adapter snapshot");

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(&shared)))));
    let engine_b = Engine::open_with_diagnostics(Some(state.path().to_path_buf()), sink)
        .expect("open engine B");
    let facade_b = engine_b.blocking();

    wait_for_diagnostic(
        &shared,
        "adapter snapshot could not be read",
        Duration::from_secs(10),
        "the degraded-adoption diagnostic",
    );
    assert_eq!(
        diagnostic_count(&shared, "adapter snapshot could not be read"),
        1,
        "exactly ONE degraded-adoption diagnostic (surfaced, not spammed)"
    );

    // Past the ORIGINAL window's expiry (A's start + 2500ms): still Running —
    // with no declared check supervised, nothing expires the instance.
    let deadline = began + Duration::from_millis(2500 + 1500);
    let now = Instant::now();
    if now < deadline {
        std::thread::sleep(deadline - now);
    }
    let status = facade_b.instance_status("hollow-hb").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Running,
        "the degraded adoption supervises PROCESS liveness only — no declared check \
         may fail the instance"
    );

    facade_b
        .stop("hollow-hb", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn adopted_instance_with_an_unreadable_manifest_keeps_process_liveness() {
    // Review round 1 (V3), inner arm: the snapshot READS, but the manifest it
    // points at was deleted since registration — the liveness re-resolution
    // fails, and the same ONE-diagnostic degradation applies: the adoption
    // succeeds, the instance is re-held with PROCESS liveness only, and it
    // stays Running past its original window (nothing declared to expire it).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "hollow2-hb",
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
                "hollow2-hb",
                &AdapterRef::Manifest(manifest.path().to_path_buf()),
            )
            .unwrap();
        let started = facade_a.start_detached("hollow2-hb").unwrap();
        assert_eq!(started.state, LifecycleState::Running);
    } // engine A drops; the DETACHED agent survives.

    // Delete the manifest the snapshot points at (A is gone — the adoption's
    // re-resolution is the next reader).
    std::fs::remove_file(manifest.path().join("adapter.toml"))
        .expect("delete the registered adapter manifest");

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(&shared)))));
    let engine_b = Engine::open_with_diagnostics(Some(state.path().to_path_buf()), sink)
        .expect("open engine B");
    let facade_b = engine_b.blocking();

    wait_for_diagnostic(
        &shared,
        "declared liveness check could not be resolved",
        Duration::from_secs(10),
        "the degraded-resolution diagnostic",
    );
    assert_eq!(
        diagnostic_count(&shared, "declared liveness check could not be resolved"),
        1,
        "exactly ONE degraded-resolution diagnostic"
    );

    // Past the ORIGINAL window's expiry: still Running (process liveness only).
    let deadline = began + Duration::from_millis(2500 + 1500);
    let now = Instant::now();
    if now < deadline {
        std::thread::sleep(deadline - now);
    }
    let status = facade_b.instance_status("hollow2-hb").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Running,
        "the degraded adoption supervises PROCESS liveness only — no declared check \
         may fail the instance"
    );

    facade_b
        .stop("hollow2-hb", Some(Duration::from_secs(5)))
        .unwrap();
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
