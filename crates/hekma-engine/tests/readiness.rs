//! Integration tests for the readiness probes (contract `1.1.0`): a manifest's
//! optional `[readiness]` section gates `starting → running`. Driven through
//! the engine's PUBLIC API + blocking facade only (spine AD-2/AD-13), spawning
//! the REAL `fake_agent` helper — whose `--http-ready` / `--ready-file` /
//! `--ready-line` / `--never-ready` fixtures provide the readiness signals —
//! so the whole gate is genuinely exercised (not mocked).
//!
//! The matrix pinned here, per the story's acceptance criteria:
//! * each probe kind PASSING gates the transition (and genuinely WAITS when
//!   its signal arrives late);
//! * a probe that never passes lands the instance `failed` with the
//!   `readiness-timeout` cause naming the probe + the honest wait — TERMINAL
//!   (no Restart Policy consult, no restart), unlike a crash;
//! * an EXIT during the probe wait is the EXISTING crash path verbatim —
//!   `Crashed` cause, Restart Policy consulted, and (for `on-failure`) the
//!   restart actually RUNS via the plan the start path queues;
//! * under `capture = "off"` a `log-contains` probe is unsatisfiable and
//!   times out honestly (surfaced, never a fake pass);
//! * a manifest WITHOUT the section keeps the pre-1.1.0 start sequence
//!   byte-identical (same transitions, same causes, no added wait);
//! * a DETACHED start with a probe is bounded — `--detach` returns
//!   ready-or-failed, never wedged.

use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hekma_engine::{
    AdapterRef, DiagnosticSink, Engine, EngineOptions, LifecycleState, LogCaptureMode,
    RestartPolicy,
};
use tempfile::TempDir;

/// Write a manifest whose `[lifecycle.start]` exec is `fake_agent` + `args`,
/// with `readiness_body` (if any) appended as the raw `[readiness]` section.
fn write_manifest(dir: &Path, kind: &str, args: &[&str], readiness_body: Option<&str>) {
    let fixture = hekma_conformance::test_support::ManifestFixture::new(kind, args)
        .guaranteed_on_all_oses("interaction");
    let fixture = match readiness_body {
        Some(body) => fixture.readiness(body),
        None => fixture,
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
fn http_get_probe_passes_and_gates_running() {
    // The `http-get` probe against the fake_agent's `--http-ready` endpoint:
    // the instance reaches `running` only once the endpoint answers.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let port = free_port();
    write_manifest(
        manifest.path(),
        "websvc",
        &["--http-ready", &format!("{port}:200"), "--linger-ms", "600000"],
        Some(&format!(
            "kind = \"http-get\"\ntimeout_ms = 8000\nurl = \"http://127.0.0.1:{port}/health\"\nexpected_status = 200"
        )),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "websvc",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let started = facade.start("websvc").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // The start sequence carries NO new cause: the probe passing falls through
    // to the ordinary `AdapterReady` edge.
    let events = facade.transition_events("websvc").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.prior_state, LifecycleState::Starting);
    assert_eq!(last.new_state, LifecycleState::Running);
    assert_eq!(last.cause, hekma_engine::TransitionCause::AdapterReady);

    facade.stop("websvc", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn http_get_probe_times_out_when_nothing_answers() {
    // The endpoint never comes up: terminal readiness-timeout, the probe NAMED.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // Bind + hold a port that will NEVER serve (nothing answers the probe).
    let held = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = held.local_addr().unwrap().port();
    // A refused/backlogged connect would also do; holding the port means the
    // probe's connect either queues unread or times out — either way, never a
    // 200 inside the budget.
    write_manifest(
        manifest.path(),
        "darksvc",
        &["--linger-ms", "600000"],
        Some(&format!(
            "kind = \"http-get\"\ntimeout_ms = 1000\nurl = \"http://127.0.0.1:{port}/health\"\nexpected_status = 200"
        )),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "darksvc",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("darksvc", RestartPolicy::OnFailure)
        .unwrap();

    let err = facade.start("darksvc").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("readiness probe http-get") && msg.contains("did not pass"),
        "the timeout must name the probe: {msg}"
    );

    let state_now = facade.instance_status("darksvc").unwrap();
    assert_eq!(state_now.instance.state, LifecycleState::Failed);
    // Terminal: an on-failure policy does NOT restart a readiness timeout.
    std::thread::sleep(Duration::from_millis(1800));
    let status = facade.instance_status("darksvc").unwrap();
    assert_eq!(status.instance.state, LifecycleState::Failed);
    assert_eq!(
        status.restart_count, 0,
        "a readiness timeout must never restart (even on-failure)"
    );

    // The event log records the readiness-timeout cause with the honest wait.
    let events = facade.transition_events("darksvc").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.prior_state, LifecycleState::Starting);
    assert_eq!(last.new_state, LifecycleState::Failed);
    match &last.cause {
        hekma_engine::TransitionCause::ReadinessTimeout { probe, waited_ms } => {
            assert!(probe.contains("http-get"), "cause names the probe: {probe}");
            assert!(
                *waited_ms >= 1000 && *waited_ms <= 2500,
                "the wait is the honest budget (~1000ms + scheduling slack), got \
                 {waited_ms}ms"
            );
        }
        other => panic!("expected readiness-timeout, got {other:?}"),
    }
    let _ = held;
}

#[test]
fn file_fresh_probe_passes_when_the_file_arrives() {
    // The `file-fresh` probe: the fake_agent writes `ready.marker` into its
    // Agent Home (its cwd) after 1000ms; the probe resolves the same relative
    // path against the same Home and the instance gates on it. The delay sits
    // AFTER the ~300ms startup survival window, so the start can only complete
    // when the probe GENUINELY waited (a regression that skipped the wait
    // would return `running` at ~300ms and blow the >= 800ms bound).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "markersvc",
        &["--ready-file", "ready.marker:1000", "--linger-ms", "600000"],
        Some(
            "kind = \"file-fresh\"\ntimeout_ms = 8000\npath = \"ready.marker\"\nmax_age_ms = 8000",
        ),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "markersvc",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let began = Instant::now();
    let started = facade.start("markersvc").unwrap();
    let elapsed = began.elapsed();
    assert_eq!(started.state, LifecycleState::Running);
    assert!(
        elapsed >= Duration::from_millis(800),
        "the start must WAIT for the late marker (took {elapsed:?})"
    );
    facade
        .stop("markersvc", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn file_fresh_probe_times_out_when_the_file_never_appears() {
    // No fixture ever writes the marker: terminal readiness-timeout naming the
    // file-fresh probe.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "stalesvc",
        &["--linger-ms", "600000"],
        Some(
            "kind = \"file-fresh\"\ntimeout_ms = 1000\npath = \"never.marker\"\nmax_age_ms = 1000",
        ),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "stalesvc",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let err = facade.start("stalesvc").unwrap_err();
    assert!(
        err.to_string().contains("readiness probe file-fresh"),
        "the timeout must name the probe: {err}"
    );
    assert_eq!(
        facade.instance_status("stalesvc").unwrap().instance.state,
        LifecycleState::Failed
    );
}

#[test]
fn log_contains_probe_waits_for_the_line_and_passes() {
    // The `log-contains` probe: the fake_agent prints its readiness line
    // 1000ms in — AFTER the ~300ms startup survival window — so the start can
    // only complete when the probe GENUINELY waits (a regression that skipped
    // the wait would return `running` at ~300ms and blow the >= 900ms bound).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "chattysvc",
        &[
            "--ready-line",
            "readiness-line-a1b2c3:1000",
            "--linger-ms",
            "600000",
        ],
        Some("kind = \"log-contains\"\ntimeout_ms = 8000\nline = \"readiness-line-a1b2c3\""),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "chattysvc",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let began = Instant::now();
    let started = facade.start("chattysvc").unwrap();
    let elapsed = began.elapsed();
    assert_eq!(started.state, LifecycleState::Running);
    assert!(
        elapsed >= Duration::from_millis(900),
        "the start must WAIT for the late line (took {elapsed:?})"
    );
    facade
        .stop("chattysvc", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn log_contains_probe_times_out_when_the_line_never_arrives() {
    // The line never prints (the agent only emits its ordinary ready line):
    // terminal readiness-timeout naming the probe.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "quietsvc",
        &["--linger-ms", "600000"],
        Some("kind = \"log-contains\"\ntimeout_ms = 1000\nline = \"a line that never prints\""),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "quietsvc",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let err = facade.start("quietsvc").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("readiness probe log-contains"),
        "the timeout must name the probe: {msg}"
    );
    assert_eq!(
        facade.instance_status("quietsvc").unwrap().instance.state,
        LifecycleState::Failed
    );
}

#[test]
fn log_contains_probe_is_unsatisfiable_under_capture_off_and_times_out_honestly() {
    // Under `capture = "off"` there IS no agent log, so a `log-contains` probe
    // can never pass: it runs out its budget honestly and lands failed with
    // the probe named — surfaced, never a fake pass (graceful degradation).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "uncaptured",
        &[
            "--ready-line",
            "readiness-line-off:100",
            "--linger-ms",
            "600000",
        ],
        Some("kind = \"log-contains\"\ntimeout_ms = 1000\nline = \"readiness-line-off\""),
    );

    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            log_capture: LogCaptureMode::Off,
            ..EngineOptions::default()
        },
    )
    .expect("open engine with capture off");
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "uncaptured",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let err = facade.start("uncaptured").unwrap_err();
    assert!(
        err.to_string().contains("readiness probe log-contains"),
        "the capture-off unsatisfiability surfaces as the honest probe timeout: {err}"
    );
    assert_eq!(
        facade.instance_status("uncaptured").unwrap().instance.state,
        LifecycleState::Failed
    );
}

#[test]
fn never_ready_probe_times_out_terminal_names_the_probe_and_never_restarts() {
    // THE timeout subject: `--never-ready` announces nothing. Even under an
    // on-failure policy the readiness timeout is TERMINAL — no restart, the
    // cause carries the probe description + the honest wait, and `show`'s
    // failed-cause fallback names the probe.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "sleepy",
        &["--never-ready", "10000"],
        Some("kind = \"log-contains\"\ntimeout_ms = 1200\nline = \"never printed\""),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "sleepy",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("sleepy", RestartPolicy::OnFailure)
        .unwrap();

    let err = facade.start("sleepy").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("readiness probe log-contains") && msg.contains("never printed"),
        "the refusal names the probe + its target: {msg}"
    );
    assert_eq!(
        facade.instance_status("sleepy").unwrap().instance.state,
        LifecycleState::Failed
    );

    // The `readiness-timeout` cause lands in the event log with the honest
    // wait (the 1200ms budget + at most one poll interval / attempt slack).
    let events = facade.transition_events("sleepy").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.prior_state, LifecycleState::Starting);
    assert_eq!(last.new_state, LifecycleState::Failed);
    match &last.cause {
        hekma_engine::TransitionCause::ReadinessTimeout { probe, waited_ms } => {
            assert!(
                probe.contains("log-contains"),
                "cause names the probe: {probe}"
            );
            assert!(
                *waited_ms >= 1200 && *waited_ms <= 2700,
                "waited_ms must bracket the budget (+ scheduling slack), got {waited_ms}ms"
            );
        }
        other => panic!("expected readiness-timeout, got {other:?}"),
    }

    // The failed-cause fallback (`show`) names the probe + the wait.
    let status = facade.instance_status("sleepy").unwrap();
    let cause = status.failed_cause.unwrap_or_default();
    assert!(
        cause.contains("readiness probe log-contains") && cause.contains("ms"),
        "the failed cause names the probe + wait: {cause}"
    );

    // TERMINAL: give the cadence a couple of ticks (a queued restart would
    // fire within ~1.3s of the failure) — nothing restarts.
    std::thread::sleep(Duration::from_millis(2000));
    let status = facade.instance_status("sleepy").unwrap();
    assert_eq!(status.instance.state, LifecycleState::Failed);
    assert_eq!(
        status.restart_count, 0,
        "a readiness timeout is terminal — no restart under on-failure"
    );
    let events = facade.transition_events("sleepy").unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.cause, hekma_engine::TransitionCause::Restarted { .. })),
        "no Restarted event may exist: {events:#?}"
    );
}

#[test]
fn exit_during_probe_follows_the_crash_path_and_the_queued_restart_runs() {
    // An EXIT while the probe waits is the EXISTING crash path, verbatim: the
    // Restart Policy is consulted (`on-failure` here), the `Crashed` cause is
    // recorded, and the restart the start path QUEUED actually runs — the
    // restarted process (crash bound reached) passes the probe this time
    // (the test creates the marker between the failure and the restart).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // Crash ~400ms in (during the probe wait, which begins after the ~300ms
    // survival window), bound to the FIRST launch only.
    write_manifest(
        manifest.path(),
        "crashy-probe",
        &[
            "--crash-after-ms",
            "400",
            "--crash-times",
            "1",
            "--crash-state",
            &*manifest.path().join("crash.state").to_string_lossy(),
            "--linger-ms",
            "600000",
        ],
        Some(
            "kind = \"file-fresh\"\ntimeout_ms = 8000\npath = \"ready.marker\"\nmax_age_ms = 8000",
        ),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "crashy-probe",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("crashy-probe", RestartPolicy::OnFailure)
        .unwrap();

    // The start REFUSES (the process exited under the probe) with the crash
    // detail preserved.
    let err = facade.start("crashy-probe").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("exited during the readiness probe"),
        "the refusal is the crash detail: {msg}"
    );

    // The instance is `failed` with a `crashed` cause (NOT readiness-timeout —
    // the crash path, verbatim).
    let events = facade.transition_events("crashy-probe").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.prior_state, LifecycleState::Starting);
    assert_eq!(last.new_state, LifecycleState::Failed);
    match &last.cause {
        hekma_engine::TransitionCause::Crashed { detail } => {
            assert!(
                detail.contains("readiness probe"),
                "the crash cause names when it happened: {detail}"
            );
        }
        other => panic!("expected a crash cause, got {other:?}"),
    }

    // Make the probe satisfiable for the RESTARTED launch: the marker appears
    // only now (it did not exist during the first launch's wait).
    let status = facade.instance_status("crashy-probe").unwrap();
    let home = Path::new(&status.instance.agent_home);
    std::fs::write(home.join("ready.marker"), b"ready").expect("write the marker");

    // The QUEUED plan runs on the engine cadence: after the production 1s base
    // backoff the restart launches, passes the probe, and lands `running`.
    let state_now = wait_until_state(
        &facade,
        "crashy-probe",
        |s| s == LifecycleState::Running,
        Duration::from_secs(20),
        "the queued restart to land running after passing the probe",
    );
    assert_eq!(state_now, LifecycleState::Running);
    let status = facade.instance_status("crashy-probe").unwrap();
    assert_eq!(status.restart_count, 1, "exactly one restart ran");

    let events = facade.transition_events("crashy-probe").unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.cause, hekma_engine::TransitionCause::Restarted { .. })),
        "the restart is recorded: {events:#?}"
    );

    facade
        .stop("crashy-probe", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn no_readiness_section_start_sequence_is_byte_identical() {
    // The byte-identity PIN: a manifest WITHOUT `[readiness]` keeps the exact
    // pre-1.1.0 start sequence — the same two transitions with the same
    // causes, no probe-wait phase added (the start completes on the ordinary
    // startup survival window, well under any probe budget).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(manifest.path(), "plain", &["--linger-ms", "600000"], None);

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "plain",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let began = Instant::now();
    let started = facade.start("plain").unwrap();
    let elapsed = began.elapsed();
    assert_eq!(started.state, LifecycleState::Running);
    // No probe, no wait: the start must complete on the ~300ms survival
    // window. (Tight-but-fair bound: a regression that made the no-section
    // path wait on a probe would overshoot by seconds; the 2s ceiling still
    // tolerates CI scheduling noise on the ~300ms real work.)
    assert!(
        elapsed < Duration::from_secs(2),
        "a no-section start must not gain a wait phase (took {elapsed:?})"
    );

    let events = facade.transition_events("plain").unwrap();
    let sequence: Vec<(
        LifecycleState,
        LifecycleState,
        &hekma_engine::TransitionCause,
    )> = events
        .iter()
        .map(|e| (e.prior_state, e.new_state, &e.cause))
        .collect();
    assert_eq!(
        sequence,
        vec![
            (
                LifecycleState::Registered,
                LifecycleState::Starting,
                &hekma_engine::TransitionCause::Command {
                    command: "start".to_string()
                }
            ),
            (
                LifecycleState::Starting,
                LifecycleState::Running,
                &hekma_engine::TransitionCause::AdapterReady
            ),
        ],
        "the no-section start sequence is exactly the pre-1.1.0 one: {events:#?}"
    );

    facade.stop("plain", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn detached_start_with_a_probe_is_bounded_and_returns_ready() {
    // A DETACHED start shares the probe gate — and stays BOUNDED: it returns
    // once the probe passes (never wedged, never before the gate). The agent
    // self-exits shortly after (linger), so the test leaves no orphan.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "detached-probe",
        &["--ready-file", "ready.marker:300", "--linger-ms", "4000"],
        Some(
            "kind = \"file-fresh\"\ntimeout_ms = 8000\npath = \"ready.marker\"\nmax_age_ms = 8000",
        ),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "detached-probe",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let began = Instant::now();
    let started = facade.start_detached("detached-probe").unwrap();
    let elapsed = began.elapsed();
    assert_eq!(started.state, LifecycleState::Running);
    // The gate genuinely held the detached start until the 300ms marker
    // appeared (the survival window alone is ~300ms; the marker lands after
    // it, so a start that skipped the probe would return faster).
    assert!(
        elapsed >= Duration::from_millis(300),
        "the detached start must wait for the probe (took {elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the detached start is bounded (took {elapsed:?})"
    );
}

#[test]
fn http_get_probe_serving_a_wrong_status_times_out_and_refuses_the_start() {
    // The dark-port test's sibling: an endpoint that ANSWERS — but with the
    // WRONG status (503 while the manifest expects 200) — is NOT ready. The
    // probe must keep polling through the wrong answers and refuse the start
    // at the budget, with the readiness-timeout cause naming the probe (a
    // listening port alone must never fake a pass).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let port = free_port();
    write_manifest(
        manifest.path(),
        "wrongsvc",
        &["--http-ready", &format!("{port}:503"), "--linger-ms", "600000"],
        Some(&format!(
            "kind = \"http-get\"\ntimeout_ms = 1000\nurl = \"http://127.0.0.1:{port}/health\"\nexpected_status = 200"
        )),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "wrongsvc",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("wrongsvc", RestartPolicy::OnFailure)
        .unwrap();

    let began = Instant::now();
    let err = facade.start("wrongsvc").unwrap_err();
    let elapsed = began.elapsed();
    let msg = err.to_string();
    assert!(
        msg.contains("readiness probe http-get") && msg.contains("did not pass"),
        "the timeout must name the probe: {msg}"
    );

    // The start waited the honest budget (not a fast refuse), and the
    // instance is `failed` TERMINALLY — on-failure does not restart a
    // readiness timeout, even though the process itself is healthy.
    assert!(
        elapsed >= Duration::from_millis(1000),
        "the start must poll through the wrong statuses for the budget (took \
         {elapsed:?})"
    );
    assert_eq!(
        facade.instance_status("wrongsvc").unwrap().instance.state,
        LifecycleState::Failed
    );
    let events = facade.transition_events("wrongsvc").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.prior_state, LifecycleState::Starting);
    assert_eq!(last.new_state, LifecycleState::Failed);
    match &last.cause {
        hekma_engine::TransitionCause::ReadinessTimeout { probe, waited_ms } => {
            assert!(probe.contains("http-get"), "cause names the probe: {probe}");
            assert!(
                *waited_ms >= 1000 && *waited_ms <= 2500,
                "the wait is the honest budget (~1000ms + scheduling slack), got \
                 {waited_ms}ms"
            );
        }
        other => panic!("expected readiness-timeout, got {other:?}"),
    }
}

#[test]
fn detached_start_with_a_never_ready_probe_is_bounded_and_lands_failed() {
    // The detached-timeout leg of the bound: a DETACHED start whose probe
    // never passes returns (refuses) within the budget — `--detach` is
    // ready-OR-failed, never wedged — and the instance is TERMINALLY failed
    // (a readiness timeout has no restart, detached or not).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "detached-sleepy",
        &["--never-ready", "10000", "--linger-ms", "600000"],
        Some("kind = \"log-contains\"\ntimeout_ms = 1200\nline = \"never printed\""),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "detached-sleepy",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("detached-sleepy", RestartPolicy::OnFailure)
        .unwrap();

    let began = Instant::now();
    let err = facade.start_detached("detached-sleepy").unwrap_err();
    let elapsed = began.elapsed();
    let msg = err.to_string();
    assert!(
        msg.contains("readiness probe log-contains"),
        "the detached refusal names the probe: {msg}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the detached start must refuse within the budget, not wedge (took \
         {elapsed:?})"
    );
    assert_eq!(
        facade
            .instance_status("detached-sleepy")
            .unwrap()
            .instance
            .state,
        LifecycleState::Failed
    );

    // Terminal: the cadence gets a couple of ticks — nothing restarts, the
    // row stays `failed` (a detached start that failed its gate must not
    // leave a `starting` zombie behind for the next open to reconcile).
    std::thread::sleep(Duration::from_millis(1500));
    let status = facade.instance_status("detached-sleepy").unwrap();
    assert_eq!(status.instance.state, LifecycleState::Failed);
    assert_eq!(status.restart_count, 0, "terminal means no restart");
}

#[test]
fn detached_start_genuinely_waits_for_a_late_probe() {
    // The detached-pass leg, with the late-signal drill the attached pass
    // tests use: the marker lands at 1000ms (after the ~300ms survival
    // window), so a detached start that skipped the probe would return early
    // and blow the >= 800ms bound.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "detached-late",
        &["--ready-file", "ready.marker:1000", "--linger-ms", "4000"],
        Some(
            "kind = \"file-fresh\"\ntimeout_ms = 8000\npath = \"ready.marker\"\nmax_age_ms = 8000",
        ),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "detached-late",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let began = Instant::now();
    let started = facade.start_detached("detached-late").unwrap();
    let elapsed = began.elapsed();
    assert_eq!(started.state, LifecycleState::Running);
    assert!(
        elapsed >= Duration::from_millis(800),
        "the detached start must WAIT for the late marker (took {elapsed:?})"
    );
}

/// A `Write` adapter delegating into a shared buffer (the `diagnostic_sink.rs`
/// shape): the test reads the sink's captured bytes while the engine owns the
/// boxed writer.
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

#[test]
fn both_probe_notes_reach_the_host_sink_in_order() {
    // The dual-note pin: the start path says the probe wait BEGAN (what is
    // watched, on what budget) and, when it passes, that it PASSED after how
    // long — both routed through the host's DiagnosticSink beside every other
    // start diagnostic. The two lines arrive in that order, and both carry
    // the rendered probe — the observable ordering evidence for the AC (the
    // pass note is emitted strictly before the `starting → running`
    // transition fires).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "noted",
        &["--ready-file", "ready.marker:300", "--linger-ms", "600000"],
        Some(
            "kind = \"file-fresh\"\ntimeout_ms = 8000\npath = \"ready.marker\"\nmax_age_ms = 8000",
        ),
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(&shared)))));
    let engine = Engine::open_with_diagnostics(Some(state.path().to_path_buf()), sink)
        .expect("open engine with sink");
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "noted",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let started = facade.start("noted").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    let captured = String::from_utf8(shared.lock().unwrap().clone()).expect("utf-8 sink");
    let began = captured
        .lines()
        .find(|l| l.contains("waiting up to 8000ms for the declared readiness probe"))
        .unwrap_or_else(|| panic!("the wait-begins note must reach the sink: {captured}"));
    let passed = captured
        .lines()
        .find(|l| l.contains("readiness probe passed after") && l.contains("file-fresh"))
        .unwrap_or_else(|| panic!("the passed-after note must reach the sink: {captured}"));
    assert!(
        began.contains("file-fresh"),
        "the wait-begins note names the probe: {began}"
    );
    let began_pos = captured.find(began).expect("began in capture");
    let passed_pos = captured.find(passed).expect("passed in capture");
    assert!(
        began_pos < passed_pos,
        "wait-begins precedes passed-after: {captured}"
    );

    facade.stop("noted", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn log_contains_probe_passes_under_metadata_capture_mode() {
    // The metadata-at-rest mode keeps the RAW capture live WHILE the instance
    // runs (transient redirects; raw files unlinked at stop) — so a
    // `log-contains` probe scanning the engine-owned log PASSES there too.
    // (Paired with the capture-off test: Off is the only unsatisfiable mode.)
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "metasvc",
        &[
            "--ready-line",
            "readiness-line-meta:300",
            "--linger-ms",
            "600000",
        ],
        Some("kind = \"log-contains\"\ntimeout_ms = 8000\nline = \"readiness-line-meta\""),
    );

    let engine = Engine::open_with_options(
        Some(state.path().to_path_buf()),
        EngineOptions {
            log_capture: LogCaptureMode::Metadata,
            ..EngineOptions::default()
        },
    )
    .expect("open engine with metadata capture");
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "metasvc",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let started = facade.start("metasvc").unwrap();
    assert_eq!(started.state, LifecycleState::Running);
    facade
        .stop("metasvc", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn contract_v1_0_0_manifest_with_readiness_section_is_accepted_by_this_engine() {
    // The versioning pin, positive leg: `[readiness]` is a LINE-ADDITIVE
    // minor, so a manifest still declaring `contract_version = "1.0.0"` (a
    // manifest that predates the section is unusual, but a 1.0.0-declaring
    // manifest WITH the section is legal the moment the section exists —
    // same major, additive field) registers, validates, and its probe gates
    // the start on this 1.1.0 engine. The negative leg (a 1.0.0 ENGINE
    // refusing the section) is pinned in hekma-adapter-api's
    // `contract_v1_0_0_engine_rejects_the_readiness_section`.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "oldsig",
        &["--ready-file", "ready.marker:100", "--linger-ms", "600000"],
        Some(
            "kind = \"file-fresh\"\ntimeout_ms = 8000\npath = \"ready.marker\"\nmax_age_ms = 8000",
        ),
    );
    // Rewrite the fixture's declared contract version down to 1.0.0.
    let manifest_file = manifest.path().join("adapter.toml");
    let text = std::fs::read_to_string(&manifest_file).expect("read the fixture");
    let downgraded = text.replace(
        &format!(
            "contract_version = \"{}\"",
            hekma_adapter_api::CONTRACT_VERSION
        ),
        "contract_version = \"1.0.0\"",
    );
    assert_ne!(text, downgraded, "the version rewrite must have happened");
    std::fs::write(&manifest_file, downgraded).expect("write the downgraded manifest");

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "oldsig",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let started = facade.start("oldsig").unwrap();
    assert_eq!(started.state, LifecycleState::Running);
    facade.stop("oldsig", Some(Duration::from_secs(5))).unwrap();
}
