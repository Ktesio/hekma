//! Integration tests for story-1.6 crash detection + the Restart Policy loop
//! (AC-A / AC5), driven end-to-end through the PUBLIC async [`Engine`] + its
//! background reaper cadence (spine AD-2/AD-13) — spawning the REAL `fake_agent`
//! with `--crash-after-ms` so a genuine process crash is detected and handled by
//! policy, not mocked.
//!
//! These tests exercise the ENGINE's automatic reaper (a tokio interval task).
//! The exhaustive fast-backoff crash-loop / count / reset logic is unit-tested in
//! the supervisor module with an injected schedule (no real seconds); here we
//! prove the wiring works through the real engine: a `never`-policy crash lands
//! `failed` with a `crashed` cause and NO restart, and an `on-failure` crash is
//! automatically restarted by the reaper (observed by the restart count growing
//! and the instance running again). The `on-failure` case tolerates the
//! PRODUCTION 1s base backoff (the first restart waits ~1s) — kept to a single
//! restart so the test stays well under a few seconds.

use std::path::Path;
use std::time::{Duration, Instant};

use hemaka_engine::{AdapterRef, Engine, LifecycleState, RestartPolicy};
use tempfile::TempDir;

/// Write a manifest whose `[lifecycle.start]` exec is `fake_agent` + `args`.
fn write_fake_manifest(dir: &Path, kind: &str, args: &[&str]) {
    let bin = hemaka_conformance::fake_agent_bin();
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

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#,
        exec = bin.to_string_lossy(),
    );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

fn open(base: &TempDir) -> Engine {
    Engine::open(Some(base.path().to_path_buf())).expect("open engine")
}

/// Poll `instance_status` until `pred(state)` holds, bounded.
fn wait_until_state(
    facade: &hemaka_engine::Blocking<'_>,
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

#[test]
fn never_policy_crash_lands_failed_via_the_reaper_no_restart() {
    // AC-A / AC5: a `never`-policy instance whose process crashes is detected by
    // the engine reaper and left `failed` with a `crashed` cause; NO restart.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // Crash after 450ms (> the 300ms readiness window, so it reaches `running`
    // first), exiting with code 7.
    write_fake_manifest(
        manifest.path(),
        "crashy",
        &["--crash-after-ms", "450", "--crash-with", "7"],
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "crashy",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    // Configure `never` so the crash is terminal (no restart timing involved).
    facade
        .set_restart_policy("crashy", RestartPolicy::Never)
        .unwrap();

    let started = facade.start("crashy").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // The reaper (250ms poll) detects the ~450ms crash and lands `failed`.
    let state_now = wait_until_state(
        &facade,
        "crashy",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(10),
        "the never-policy crash to be detected as failed",
    );
    assert_eq!(state_now, LifecycleState::Failed);

    // Give the reaper a couple more polls to prove it does NOT restart a `never`
    // instance (it stays failed, restart count stays 0).
    std::thread::sleep(Duration::from_millis(600));
    let status = facade.instance_status("crashy").unwrap();
    assert_eq!(status.instance.state, LifecycleState::Failed);
    assert_eq!(status.restart_count, 0, "never policy must not restart");
    assert_eq!(status.restart_policy, RestartPolicy::Never);
    // The failed cause is a crash with the exit code preserved.
    let cause = status.failed_cause.unwrap_or_default();
    assert!(cause.contains("code 7"), "failed cause={cause}");

    // The event log records the `crashed` cause.
    let events = facade.transition_events("crashy").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.new_state, LifecycleState::Failed);
    let cause_json = serde_json::to_string(&last.cause).unwrap();
    assert!(cause_json.contains("crashed"), "cause={cause_json}");
}

#[test]
fn on_failure_crash_is_restarted_by_the_reaper() {
    // AC-A / AC4: an `on-failure` instance whose process crashes is AUTOMATICALLY
    // restarted by the engine reaper — the restart count grows and the instance
    // runs again. Tolerates the production 1s base backoff (a single restart).
    //
    // DETERMINISM (AI-49): the agent crashes EXACTLY ONCE, then the restarted
    // process lingers instead of crashing again. `--crash-times 1` keeps a persisted
    // launch counter in a file that survives the restart (a fresh process), so the
    // FIRST launch crashes ~450ms in and the SECOND (restarted) launch stays up.
    // This removes the parallel-load race the older single-`--crash-after-ms`
    // version had: with the agent crashing on EVERY launch, the reaper could detect
    // a SECOND crash and bump the persisted restart count to 2 in the window between
    // this test observing the first `Restarted` event and reading the count — a
    // flaky `restart_count == 1`. With a single crash the count is stably 1 and the
    // final stop cannot race a second crash.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // The cross-restart crash counter lives in the manifest dir (stable across the
    // reaper's restart, which re-execs the SAME manifest args). Crash once ~450ms
    // after the first launch, then linger.
    let crash_state = manifest.path().join("crash-count");
    write_fake_manifest(
        manifest.path(),
        "recovering",
        &[
            "--crash-after-ms",
            "450",
            "--crash-times",
            "1",
            "--crash-state",
            &crash_state.to_string_lossy(),
        ],
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "recovering",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    // on-failure is the default, but be explicit.
    facade
        .set_restart_policy("recovering", RestartPolicy::OnFailure)
        .unwrap();

    facade.start("recovering").unwrap();

    // Wait for the reaper to (1) detect the crash → `failed`, then (2) ACTUALLY
    // perform the restart after the production 1s base backoff — proven by a
    // `Restarted` event appearing in the log (NOT merely the count bumping, which
    // happens at crash-detection time before the backoff). Generous timeout to
    // accommodate the 1s backoff + readiness on a loaded CI runner. We simply
    // observe the restart land; note that a mid-backoff instance is `failed`, and
    // there is no `failed → stopping` edge this story, so it cannot be stopped until
    // it is `running` again (the restart is not pre-emptable during the backoff).
    let deadline = Instant::now() + Duration::from_secs(30);
    let restart_evt = loop {
        let events = facade.transition_events("recovering").unwrap();
        if let Some(evt) = events
            .iter()
            .find(|e| matches!(&e.cause, hemaka_engine::TransitionCause::Restarted { .. }))
        {
            break evt.clone();
        }
        assert!(
            Instant::now() < deadline,
            "the on-failure crash was never restarted (no Restarted event)"
        );
        std::thread::sleep(Duration::from_millis(100));
    };

    // The `restarted` event records the count + the ≥1s backoff waited (both are
    // recorded DATA on the event, not wall-clock measurements — parallel-load safe).
    match &restart_evt.cause {
        hemaka_engine::TransitionCause::Restarted { count, waited_ms } => {
            assert_eq!(*count, 1);
            assert!(
                *waited_ms >= 1000,
                "the first restart waits the 1s base backoff"
            );
        }
        _ => unreachable!(),
    }

    // The restart count is durably 1 AND the instance is running again. Poll both to
    // the terminal, stable post-restart condition (the restarted process no longer
    // crashes, so this converges and holds): `running` with restart_count == 1. A
    // generous bound tolerates a loaded runner finishing the restart's readiness.
    let converge = Instant::now() + Duration::from_secs(30);
    loop {
        let status = facade.instance_status("recovering").unwrap();
        if status.instance.state == LifecycleState::Running && status.restart_count == 1 {
            break;
        }
        assert!(
            Instant::now() < converge,
            "expected the restarted instance to settle running with restart_count == 1 \
             (state: {}, count: {})",
            status.instance.state,
            status.restart_count
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Stop it (now stably `running`) to bound the test — a stop is only effective
    // once the instance is running again, which it is after the observed restart,
    // and (unlike the old every-launch-crash version) it cannot race a second crash.
    let _ = facade.stop("recovering", Some(Duration::from_secs(5)));
}
