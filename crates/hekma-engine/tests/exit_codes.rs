//! Integration tests for the declared exit-code classes and the stable-window
//! reset (contract `1.1.0`, story 18-3). A manifest's optional `[exit_codes]`
//! section gives the agent's own exit codes DECLARED MEANING:
//!
//! * `restart_requested = [codes]` — the agent asked for a relaunch: the
//!   crash-path dance (terminal `running → failed` + restart plan, backoff
//!   verbatim) with the `RestartRequested { code }` cause, but the
//!   consecutive-failure counter is NOT charged — an exit-75 loop can NEVER
//!   trip the crash-loop terminal (the operator declared the class), while
//!   the backoff still grows toward its cap;
//! * `clean = [codes]` — the agent finished on purpose: `running → stopped`
//!   with `CleanExit { code }`, settled like a requested stop, no plan, no
//!   counter action (the operator's next fresh start is the counter reset,
//!   exactly as it has always been);
//! * the STABLE-WINDOW reset is ENGINE-owned: a run that held `running`
//!   continuously for ≥ the window (30 s in production, scaled here through
//!   the doc-hidden `EngineOptions::stable_run_window_override` test seam)
//!   resets the counter at its crash — this crash charges as failure #1, the
//!   reset NAMED in the detail. A pause-interval never counts (the anchor
//!   restamps at every resume); an adopted instance's anchor starts at
//!   ADOPTION (pre-adoption run time proves nothing).
//!
//! A manifest WITHOUT the section keeps the pre-18-3 behavior byte-identical
//! (pinned: same transitions, same causes, same counter arithmetic), and
//! exits during `starting` are LAUNCH failures — never classified.
//!
//! The mutation killers required by the spec, and the tests that carry them:
//! * restart_requested must not charge →
//!   `restart_requested_exit_relaunches_without_charging_the_counter`
//! * the exit-75 loop must never terminal →
//!   `exit_requested_loop_never_trips_the_crash_loop_terminal`
//! * the stable reset must reset-then-charge →
//!   `crash_after_a_stable_run_resets_then_charges_as_failure_one`
//! * the pause interval must not count →
//!   `pause_interval_never_counts_toward_stability`
//! * a clean exit must not plan → `clean_exit_lands_stopped_settled_without_a_plan`
//! * `never` must outrank the ask → `never_policy_outranks_the_restart_request`

use std::path::Path;
use std::time::{Duration, Instant};

use hekma_engine::{
    AdapterRef, Engine, EngineOptions, LifecycleState, RestartPolicy, TransitionCause,
};
use tempfile::TempDir;

/// Write a manifest whose `[lifecycle.start]` exec is `fake_agent` + `args`,
/// with `exit_codes_body` (if any) appended as the raw `[exit_codes]` section
/// and `pause_guaranteed` adding the pause capability (the pause drill needs
/// it; nobody else).
fn write_manifest(
    dir: &Path,
    kind: &str,
    args: &[&str],
    exit_codes_body: Option<&str>,
    pause_guaranteed: bool,
) {
    let fixture = hekma_conformance::test_support::ManifestFixture::new(kind, args)
        .guaranteed_on_all_oses("interaction");
    let fixture = match exit_codes_body {
        Some(body) => fixture.exit_codes(body),
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

/// Open an engine with the STABLE-RUN WINDOW scaled to `window_ms` (the
/// doc-hidden test seam — production is pinned at 30 s and no test here may
/// sleep that long). The override changes ONLY the window's LENGTH; the
/// reset semantics are the production ones.
fn open_with_window(base: &TempDir, window_ms: u64) -> Engine {
    Engine::open_with_options(
        Some(base.path().to_path_buf()),
        EngineOptions {
            stable_run_window_override: Some(Duration::from_millis(window_ms)),
            ..EngineOptions::default()
        },
    )
    .expect("open engine with the scaled stable-run window")
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

#[test]
fn restart_requested_exit_relaunches_without_charging_the_counter() {
    // MUTATION KILLER (a): the restart-requested path runs the crash-path
    // dance (terminal failed + relaunch, backoff verbatim) but must NEVER
    // touch the consecutive-failure counter. Two full exit-75 recycles with
    // the count pinned at 0 after EACH: a mutation that charged would land
    // the count at 1, then 2. The cause names the declared class.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "relauncher",
        &["--exit-with", "75:600", "--linger-ms", "600000"],
        Some("restart_requested = [75]"),
        false,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "relauncher",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("relauncher", RestartPolicy::OnFailure)
        .unwrap();

    let started = facade.start("relauncher").unwrap();
    assert_eq!(started.state, LifecycleState::Running);
    let count_before = facade.instance_status("relauncher").unwrap().restart_count;
    assert_eq!(count_before, 0, "a fresh start holds the count at 0");

    // Recycle 1: the exit-75 lands `failed` with the RestartRequested cause,
    // then the policy relaunch runs on the base backoff.
    wait_until_state(
        &facade,
        "relauncher",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(15),
        "the first exit-75 to land failed",
    );
    let events = facade.transition_events("relauncher").unwrap();
    let failed = events
        .iter()
        .rev()
        .find(|e| e.new_state == LifecycleState::Failed)
        .expect("a failed transition");
    match &failed.cause {
        TransitionCause::RestartRequested { code, detail } => {
            assert_eq!(*code, 75, "the cause carries the observed code");
            let detail = detail.as_deref().unwrap_or_default();
            assert!(
                detail.contains("declared class `restart_requested`"),
                "the cause names the declared class: {detail}"
            );
        }
        other => panic!("expected a restart-requested cause, got {other:?}"),
    }
    wait_until_state(
        &facade,
        "relauncher",
        |s| s == LifecycleState::Running,
        Duration::from_secs(20),
        "the first requested relaunch to land running",
    );
    let count = facade.instance_status("relauncher").unwrap().restart_count;
    assert_eq!(count, 0, "recycle 1 must not charge the counter");

    // Recycle 2: same drill — the count is STILL 0 (a charging mutant reads
    // 2 here).
    wait_until_state(
        &facade,
        "relauncher",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(15),
        "the second exit-75 to land failed",
    );
    wait_until_state(
        &facade,
        "relauncher",
        |s| s == LifecycleState::Running,
        Duration::from_secs(20),
        "the second requested relaunch to land running",
    );
    let count = facade.instance_status("relauncher").unwrap().restart_count;
    assert_eq!(count, 0, "recycle 2 must not charge the counter either");

    // No crash-loop anywhere, no clean-exit edge: the causes are exactly the
    // declared story.
    let events = facade.transition_events("relauncher").unwrap();
    assert!(
        events
            .iter()
            .all(|e| !matches!(e.cause, TransitionCause::CleanExit { .. })),
        "a restart-requested exit is never a clean exit: {events:#?}"
    );
    assert!(
        !format!("{events:?}").contains("crash-loop"),
        "two requested recycles must never approach the crash loop: {events:#?}"
    );

    facade
        .stop("relauncher", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn exit_requested_loop_never_trips_the_crash_loop_terminal() {
    // MUTATION KILLER (b) — the footgun answered by test: MAX+1 = 6
    // consecutive exit-75 runs STILL plan restarts. A mutant that charged
    // the counter would trip the 5-failure crash-loop terminal at the fifth
    // failure and never relaunch a sixth time. The declared recycle chain
    // simply keeps going (the backoff grows toward its cap; the RATE stays
    // bounded — pinned at the unit level).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "looper",
        &["--exit-with", "75:600", "--linger-ms", "600000"],
        Some("restart_requested = [75]"),
        false,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "looper",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("looper", RestartPolicy::OnFailure)
        .unwrap();

    let started = facade.start("looper").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // Six full requested recycles. The delays grow 1s, 2s, 4s, 8s, 16s, 32s
    // (the streak-paced backoff) — the final bound carries the 32s leg, and
    // the GAP between the last failure and its relaunch is MEASURED: a
    // mutant that reset the pace every relaunch (the delay never growing)
    // would recycle in ~1.3s where the contract holds ~32s.
    let mut last_gap = Duration::ZERO;
    for recycle in 1..=6 {
        wait_until_state(
            &facade,
            "looper",
            |s| s == LifecycleState::Failed,
            Duration::from_secs(15),
            &format!("exit-75 failure #{recycle} to land failed"),
        );
        let failed_at = Instant::now();
        let bound = if recycle == 6 { 45 } else { 20 };
        wait_until_state(
            &facade,
            "looper",
            |s| s == LifecycleState::Running,
            Duration::from_secs(bound),
            &format!("requested relaunch #{recycle} to land running"),
        );
        last_gap = failed_at.elapsed();
        let status = facade.instance_status("looper").unwrap();
        assert_eq!(
            status.restart_count, 0,
            "recycle {recycle}: the counter must never charge on the requested path"
        );
    }
    // The sixth relaunch waited the streak-paced leg (32s, less the tick
    // slop between the exit and the failed-state observation — a lower
    // bound, so CI slowness only helps). A flat-pace mutant lands ~1.3s.
    assert!(
        last_gap >= Duration::from_secs(25),
        "the requested-relaunch backoff must GROW with the streak (the sixth \
         leg is paced at 32s): measured {last_gap:?}"
    );

    // The whole chain is restart-requested causes; the crash-loop terminal
    // NEVER appears (the operator declared the class; docs state that
    // contract honestly).
    let events = facade.transition_events("looper").unwrap();
    let failures = events
        .iter()
        .filter(|e| e.new_state == LifecycleState::Failed)
        .count();
    assert!(
        failures >= 6,
        "the drill drove six failures; the log holds {failures}"
    );
    for event in events
        .iter()
        .filter(|e| e.new_state == LifecycleState::Failed)
    {
        match &event.cause {
            TransitionCause::RestartRequested { code, .. } => {
                assert_eq!(*code, 75);
            }
            other => panic!("a non-requested cause infiltrated the loop: {other:?}"),
        }
    }

    facade.stop("looper", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn crash_after_a_stable_run_resets_then_charges_as_failure_one() {
    // MUTATION KILLER (c): a run that held `running` ≥ the (scaled) window
    // resets the counter BEFORE its crash charges — the crash is failure #1,
    // and the reset is NAMED in the crash detail. Every run here clears the
    // window, so EVERY crash resets the history it inherits and re-charges
    // as #1: the count reads 1 after each. A charge-only mutant (no reset)
    // reads 1, 2, 3; a reset-without-charge mutant reads 0; only
    // reset-then-charge reads 1 every time — with the reset NAMED.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // Window 2s; each run holds `running` ~3.7s (the 4s crash deadline less
    // the ~300ms survival window) — comfortably above it.
    write_manifest(
        manifest.path(),
        "stable-crasher",
        &[
            "--crash-after-ms",
            "4000",
            "--crash-with",
            "7",
            "--linger-ms",
            "600000",
        ],
        Some("restart_requested = [75]"),
        false,
    );

    let engine = open_with_window(&state, 2_000);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "stable-crasher",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("stable-crasher", RestartPolicy::OnFailure)
        .unwrap();

    let started = facade.start("stable-crasher").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    for crash in 1..=3u32 {
        wait_until_state(
            &facade,
            "stable-crasher",
            |s| s == LifecycleState::Failed,
            Duration::from_secs(15),
            &format!("stable crash #{crash} to land failed"),
        );
        let events = facade.transition_events("stable-crasher").unwrap();
        let failed = events
            .iter()
            .rev()
            .find(|e| e.new_state == LifecycleState::Failed)
            .unwrap();
        match &failed.cause {
            TransitionCause::Crashed { detail } => {
                assert!(
                    detail.starts_with("process exited unexpectedly with code 7"),
                    "crash #{crash}: the detail leads with the exit: {detail}"
                );
                assert!(
                    detail.contains("the previous run was stable for")
                        && detail.contains("consecutive-failure count reset"),
                    "crash #{crash}: the reset is NAMED in the crash detail: {detail}"
                );
            }
            other => panic!("expected a crashed cause, got {other:?}"),
        }
        // The reset pins `next` at 1, so every backoff is the base 1s leg.
        wait_until_state(
            &facade,
            "stable-crasher",
            |s| s == LifecycleState::Running,
            Duration::from_secs(20),
            &format!("relaunch #{crash} to land running"),
        );
        let status = facade.instance_status("stable-crasher").unwrap();
        assert_eq!(
            status.restart_count, 1,
            "stable crash #{crash}: reset-then-charge lands exactly 1"
        );
    }

    facade
        .stop("stable-crasher", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn pause_interval_never_counts_toward_stability() {
    // MUTATION KILLER (d): the stability anchor RESTAMPS at every resume —
    // a pause longer than the window contributes nothing. The drill builds
    // count 1 with a young crash, then pauses the second run LONGER than
    // the window, resumes, and lets the run crash almost immediately: the
    // post-resume run is YOUNG (a reset would be a lie), so the counter
    // charges to 2. A mutant that kept the pre-pause anchor reads the
    // paused wall-clock as "stable for > window" and resets (count 1).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // Window 3s; the young crash (~1.5s run) stays under it. The fixture's
    // crash deadline is WALL-CLOCK from its own spawn: after a suspension
    // longer than the remaining budget, the process exits right after the
    // resume (unix) — or, where a pause cannot suspend (windows), while the
    // row still says paused, which rides the same charged crash path with
    // NO anchor (the state gate) — both shapes land the honest no-reset.
    write_manifest(
        manifest.path(),
        "frozen-crasher",
        &[
            "--crash-after-ms",
            "1800",
            "--crash-with",
            "7",
            "--linger-ms",
            "600000",
        ],
        Some("restart_requested = [75]"),
        true,
    );

    let engine = open_with_window(&state, 3_000);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "frozen-crasher",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("frozen-crasher", RestartPolicy::OnFailure)
        .unwrap();

    let started = facade.start("frozen-crasher").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // History: the first young crash charges the counter to 1.
    wait_until_state(
        &facade,
        "frozen-crasher",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(15),
        "the first young crash to land failed",
    );
    wait_until_state(
        &facade,
        "frozen-crasher",
        |s| s == LifecycleState::Running,
        Duration::from_secs(20),
        "the first relaunch to land running",
    );
    let status = facade.instance_status("frozen-crasher").unwrap();
    assert_eq!(status.restart_count, 1, "history: the count is 1");

    // Pause LONGER than the window, resume, and let the run crash almost
    // immediately (the fixture's wall-clock deadline is already past).
    let paused = facade.pause("frozen-crasher").unwrap();
    assert_eq!(paused.state, LifecycleState::Paused);
    std::thread::sleep(Duration::from_millis(3_500));
    facade.resume("frozen-crasher").unwrap();

    wait_until_state(
        &facade,
        "frozen-crasher",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(15),
        "the post-resume crash to land failed",
    );
    // NO reset anywhere in the causes: the pause interval bought nothing.
    let events = facade.transition_events("frozen-crasher").unwrap();
    assert!(
        !format!("{events:?}").contains("consecutive-failure count reset"),
        "a pause interval must never contribute stability: {events:#?}"
    );
    wait_until_state(
        &facade,
        "frozen-crasher",
        |s| s == LifecycleState::Running,
        Duration::from_secs(25),
        "the post-pause relaunch to land running",
    );
    let status = facade.instance_status("frozen-crasher").unwrap();
    assert_eq!(
        status.restart_count, 2,
        "the post-pause crash charged WITHOUT a reset (an anchor that \
         counted the pause would have reset to 1)"
    );

    facade
        .stop("frozen-crasher", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn clean_exit_lands_stopped_settled_without_a_plan() {
    // MUTATION KILLER (e): a declared-clean exit is the requested-stop
    // shape — `running → stopped` with the CleanExit cause, the record
    // settled, NO restart plan (the instance stays stopped), and NO counter
    // action. The next fresh start is the operator's reset, exactly as it
    // has always been — and the clean exit is repeatable.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "finisher",
        &["--exit-with", "0:600", "--linger-ms", "600000"],
        Some("clean = [0]"),
        false,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "finisher",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    // OnFailure is the shipped default and the sharp test: a POLICY that
    // restarts failures must still NOT restart a declared-clean exit.
    facade
        .set_restart_policy("finisher", RestartPolicy::OnFailure)
        .unwrap();

    let started = facade.start("finisher").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    let final_state = wait_until_state(
        &facade,
        "finisher",
        |s| s == LifecycleState::Stopped,
        Duration::from_secs(15),
        "the declared exit-0 to land stopped",
    );
    assert_eq!(final_state, LifecycleState::Stopped);
    let events = facade.transition_events("finisher").unwrap();
    let stopped = events
        .iter()
        .rev()
        .find(|e| e.new_state == LifecycleState::Stopped)
        .expect("a stopped transition");
    assert_eq!(stopped.prior_state, LifecycleState::Running);
    match &stopped.cause {
        TransitionCause::CleanExit { code } => {
            assert_eq!(*code, 0, "the cause carries the observed code");
        }
        other => panic!("expected a clean-exit cause, got {other:?}"),
    }
    assert!(
        !events.iter().any(|e| e.new_state == LifecycleState::Failed),
        "a clean exit is never a failure: {events:#?}"
    );
    let status = facade.instance_status("finisher").unwrap();
    assert_eq!(
        status.restart_count, 0,
        "no counter action follows a clean exit"
    );

    // No plan: the instance STAYS stopped (a restart-planning mutant would
    // relaunch it within the base backoff).
    std::thread::sleep(Duration::from_millis(2_500));
    let status = facade.instance_status("finisher").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Stopped,
        "a clean exit plans no restart"
    );

    // The settle: a fresh start works (the record settled like a requested
    // stop), and the clean exit is repeatable — the second run also finishes
    // on purpose, still with no counter action.
    let restarted = facade.start("finisher").unwrap();
    assert_eq!(restarted.state, LifecycleState::Running);
    let final_state = wait_until_state(
        &facade,
        "finisher",
        |s| s == LifecycleState::Stopped,
        Duration::from_secs(15),
        "the second declared exit-0 to land stopped",
    );
    assert_eq!(final_state, LifecycleState::Stopped);
    let status = facade.instance_status("finisher").unwrap();
    assert_eq!(status.restart_count, 0, "still no counter action");
}

#[test]
fn never_policy_outranks_the_restart_request() {
    // MUTATION KILLER (f): `RestartPolicy::Never` is the operator's
    // instrument and WINS over the adapter's ask — terminal `failed` with
    // the RestartRequested cause (naming BOTH the class and the policy
    // verdict), no plan (the instance stays failed), no counter action.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "polite",
        &["--exit-with", "75:600", "--linger-ms", "600000"],
        Some("restart_requested = [75]"),
        false,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "polite",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("polite", RestartPolicy::Never)
        .unwrap();

    let started = facade.start("polite").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    let final_state = wait_until_state(
        &facade,
        "polite",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(15),
        "the exit-75 to land failed under the never policy",
    );
    assert_eq!(final_state, LifecycleState::Failed);
    let events = facade.transition_events("polite").unwrap();
    let failed = events
        .iter()
        .rev()
        .find(|e| e.new_state == LifecycleState::Failed)
        .unwrap();
    match &failed.cause {
        TransitionCause::RestartRequested { code, detail } => {
            assert_eq!(*code, 75);
            let detail = detail.as_deref().unwrap_or_default();
            assert!(
                detail.contains("declared class `restart_requested`"),
                "the cause names the class: {detail}"
            );
            assert!(
                detail.contains("never") && detail.contains("outranks"),
                "the cause carries the policy verdict: {detail}"
            );
        }
        other => panic!("expected a restart-requested cause, got {other:?}"),
    }

    // No plan: the instance STAYS failed (a planning mutant relaunches it
    // within the base backoff), and the counter never charged.
    std::thread::sleep(Duration::from_millis(2_500));
    let status = facade.instance_status("polite").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Failed,
        "the operator's never policy outranks the adapter's ask"
    );
    assert_eq!(
        status.restart_count, 0,
        "no counter action on the terminal path"
    );
    // `show`'s failed-cause fold names the class through the existing
    // projection (the AC's "no new verbs" surface).
    assert!(
        status.failed_cause.as_deref().is_some_and(
            |cause| cause.contains("exit code 75") && cause.contains("restart_requested")
        ),
        "the failed-cause fold names the class + code: {:?}",
        status.failed_cause
    );
}

#[test]
fn young_run_crash_increments_without_a_reset() {
    // The complement of the stable-reset drill: with the window scaled FAR
    // above every run's length, consecutive crashes charge 1, 2 — the
    // counter is history, not a per-crash constant. The manifest DECLARES
    // an `[exit_codes]` section (restart_requested = [75]) while the crash
    // code (7) is UNCLASSIFIED: a declared section changes nothing for an
    // unclassified exit.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "young-crasher",
        &[
            "--crash-after-ms",
            "800",
            "--crash-with",
            "7",
            "--linger-ms",
            "600000",
        ],
        Some("restart_requested = [75]"),
        false,
    );

    let engine = open_with_window(&state, 10_000);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "young-crasher",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("young-crasher", RestartPolicy::OnFailure)
        .unwrap();

    let started = facade.start("young-crasher").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    for attempt in 1..=2u32 {
        wait_until_state(
            &facade,
            "young-crasher",
            |s| s == LifecycleState::Failed,
            Duration::from_secs(15),
            &format!("young crash #{attempt} to land failed"),
        );
        let events = facade.transition_events("young-crasher").unwrap();
        let failed = events
            .iter()
            .rev()
            .find(|e| e.new_state == LifecycleState::Failed)
            .unwrap();
        match &failed.cause {
            TransitionCause::Crashed { detail } => {
                assert_eq!(
                    detail.as_str(),
                    "process exited unexpectedly with code 7",
                    "an unclassified exit in a DECLARING manifest charges verbatim"
                );
            }
            other => panic!("expected a crashed cause, got {other:?}"),
        }
        let bound = if attempt == 1 { 20 } else { 25 };
        wait_until_state(
            &facade,
            "young-crasher",
            |s| s == LifecycleState::Running,
            Duration::from_secs(bound),
            &format!("relaunch #{attempt} to land running"),
        );
        let status = facade.instance_status("young-crasher").unwrap();
        assert_eq!(
            status.restart_count, attempt,
            "a young run's crash increments as today"
        );
    }

    facade
        .stop("young-crasher", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn no_exit_codes_section_crash_path_is_byte_identical() {
    // The byte-identity PIN: a manifest WITHOUT `[exit_codes]` keeps the
    // exact pre-18-3 crash + restart behavior through TWO full charge
    // cycles — the same transitions with the same causes (the crash detail
    // verbatim, no reset suffix, no restart-requested anywhere), the
    // counter charging 1 then 2, and NO extra engine options (the
    // production 30s window, under which these young runs can never reset).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "plain",
        &[
            "--crash-after-ms",
            "800",
            "--crash-with",
            "7",
            "--linger-ms",
            "600000",
        ],
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
    facade
        .set_restart_policy("plain", RestartPolicy::OnFailure)
        .unwrap();

    let started = facade.start("plain").unwrap();
    assert_eq!(started.state, LifecycleState::Running);
    wait_until_state(
        &facade,
        "plain",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(15),
        "the first crash to land failed",
    );
    wait_until_state(
        &facade,
        "plain",
        |s| s == LifecycleState::Running,
        Duration::from_secs(20),
        "the first policy restart to land running",
    );
    wait_until_state(
        &facade,
        "plain",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(25),
        "the second crash to land failed",
    );

    let events = facade.transition_events("plain").unwrap();
    let mut seen_restarted = 0usize;
    for (index, event) in events.iter().enumerate() {
        match (&event.cause, event.prior_state, event.new_state) {
            (
                TransitionCause::Command { command },
                LifecycleState::Registered,
                LifecycleState::Starting,
            )
            | (
                TransitionCause::Command { command },
                LifecycleState::Failed,
                LifecycleState::Starting,
            ) => {
                assert_eq!(command, "start", "edge {index}");
            }
            (TransitionCause::AdapterReady, LifecycleState::Starting, LifecycleState::Running) => {}
            (
                TransitionCause::Crashed { detail },
                LifecycleState::Running,
                LifecycleState::Failed,
            ) => {
                assert_eq!(
                    detail.as_str(),
                    "process exited unexpectedly with code 7",
                    "edge {index}: the no-section crash detail is verbatim pre-18-3"
                );
            }
            (
                TransitionCause::Restarted { count, .. },
                LifecycleState::Starting,
                LifecycleState::Running,
            ) => {
                seen_restarted += 1;
                assert_eq!(*count, seen_restarted as u32, "edge {index}");
            }
            (cause, prior, new) => {
                panic!("edge {index} is not pre-18-3 shape: {prior:?} → {new:?} via {cause:?}");
            }
        }
    }
    assert_eq!(
        seen_restarted, 1,
        "exactly one policy restart ran (two crashes, the second not yet restarted): {events:#?}"
    );
    assert!(
        !format!("{events:?}").contains("consecutive-failure count reset"),
        "no reset may be named on the no-section path: {events:#?}"
    );
    let status = facade.instance_status("plain").unwrap();
    assert_eq!(status.restart_count, 2, "the counter charged 1 then 2");

    // No stop: the instance ends TERMINAL failed (the launch-path honest
    // terminal — `stop` is an InvalidTransition there), with no live process
    // left behind (the crash already reaped it; a late queued relaunch, if
    // any, dies with this engine's kill-on-drop).
}

#[test]
fn exit_during_starting_is_never_classified() {
    // The classes classify RUNS, not launches: an exit during `starting`
    // (the survival window) is a LAUNCH failure on today's path — under a
    // declaring manifest, a code that would be `restart_requested` lands
    // the launch-error terminal (no RestartRequested cause, no plan), and
    // a code that would be `clean` lands `failed`, NOT `stopped`.
    let state = TempDir::new().unwrap();
    let manifest_rr = TempDir::new().unwrap();
    write_manifest(
        manifest_rr.path(),
        "websvc",
        &["--exit-fast", "75"],
        Some("restart_requested = [75]"),
        false,
    );
    let manifest_clean = TempDir::new().unwrap();
    write_manifest(
        manifest_clean.path(),
        "websvc",
        &["--exit-fast", "0"],
        Some("clean = [0]"),
        false,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    for (name, path) in [
        ("launch-75", manifest_rr.path()),
        ("launch-0", manifest_clean.path()),
    ] {
        facade
            .register_with_adapter(name, &AdapterRef::Manifest(path.to_path_buf()))
            .unwrap();
        facade
            .set_restart_policy(name, RestartPolicy::OnFailure)
            .unwrap();

        let err = facade.start(name).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exited immediately during startup"),
            "{name}: the launch-failure refusal preserves the launch shape: {msg}"
        );
        let status = facade.instance_status(name).unwrap();
        assert_eq!(
            status.instance.state,
            LifecycleState::Failed,
            "{name}: a launch failure lands failed"
        );
        let events = facade.transition_events(name).unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.cause, TransitionCause::RestartRequested { .. })),
            "{name}: a launch-phase exit is never classified restart-requested: {events:#?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.cause, TransitionCause::CleanExit { .. })),
            "{name}: a launch-phase exit is never classified clean: {events:#?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| e.new_state == LifecycleState::Stopped),
            "{name}: a launch-phase exit never lands stopped: {events:#?}"
        );
    }

    // No plan follows a launch failure: both instances STAY failed (under
    // OnFailure — the launch path's honest terminals are terminal).
    std::thread::sleep(Duration::from_millis(2_500));
    for name in ["launch-75", "launch-0"] {
        let status = facade.instance_status(name).unwrap();
        assert_eq!(
            status.instance.state,
            LifecycleState::Failed,
            "{name}: no restart follows a launch failure"
        );
    }
}

#[test]
fn adopted_instance_crash_does_not_credit_pre_adoption_time() {
    // The adopted-instance honesty: the stability anchor starts at ADOPTION
    // — the pre-adoption run time is not observable, so an adopted run's
    // crash may only reset after THIS engine has itself watched ≥ the
    // window. The drill: run 1 crashes young under engine A (count 1); the
    // DETACHED relaunch (run 2) survives A's drop; engine B adopts it with
    // a FRESH anchor and watches well UNDER the window before run 2
    // crashes — the counter charges to 2, naming NO reset. A mutant that
    // let adoption inherit the process's whole age as stability would read
    // a reset (count 1).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // Window 5s: run 1 (~3.7s) stays under it; B watches ~3.4s of run 2 —
    // also under it. The detach keeps run 2 alive across A's drop.
    write_manifest(
        manifest.path(),
        "adopted-crasher",
        &[
            "--crash-after-ms",
            "4000",
            "--crash-with",
            "7",
            "--linger-ms",
            "600000",
        ],
        None,
        false,
    );

    let began = Instant::now();
    {
        let engine_a = open_with_window(&state, 5_000);
        let facade_a = engine_a.blocking();
        facade_a
            .register_with_adapter(
                "adopted-crasher",
                &AdapterRef::Manifest(manifest.path().to_path_buf()),
            )
            .unwrap();
        facade_a
            .set_restart_policy("adopted-crasher", RestartPolicy::OnFailure)
            .unwrap();
        let started = facade_a.start_detached("adopted-crasher").unwrap();
        assert_eq!(started.state, LifecycleState::Running);

        // Run 1 crashes young (count 1); the policy relaunch (run 2, still
        // detached) lands running under A.
        wait_until_state(
            &facade_a,
            "adopted-crasher",
            |s| s == LifecycleState::Failed,
            Duration::from_secs(15),
            "run 1's young crash to land failed",
        );
        wait_until_state(
            &facade_a,
            "adopted-crasher",
            |s| s == LifecycleState::Running,
            Duration::from_secs(20),
            "the detached relaunch (run 2) to land running",
        );
        let status = facade_a.instance_status("adopted-crasher").unwrap();
        assert_eq!(status.restart_count, 1, "run 1 charged young");
        // A drops with run 2 freshly running (~0.3s of run-2 age watched).
        std::thread::sleep(Duration::from_millis(300));
    } // engine A drops; the DETACHED run 2 survives (disarmed handle).

    // Engine B adopts run 2 with the same scaled window: the anchor starts
    // at ADOPTION.
    let open_started = Instant::now();
    let engine_b = open_with_window(&state, 5_000);
    let facade_b = engine_b.blocking();
    let adopted_at = Instant::now();
    let adoption_epsilon = adopted_at.saturating_duration_since(open_started);
    let status = facade_b.instance_status("adopted-crasher").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Running,
        "run 2 is re-held running under B"
    );
    assert_eq!(
        status.restart_count, 1,
        "the adopted record carries run 1's charge"
    );

    // Run 2 crashes (its own 4s deadline), B-watched well under the window:
    // charged WITHOUT a reset.
    wait_until_state(
        &facade_b,
        "adopted-crasher",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(15),
        "run 2's crash to land failed under B",
    );
    let events = facade_b.transition_events("adopted-crasher").unwrap();
    assert!(
        !format!("{events:?}").contains("consecutive-failure count reset"),
        "the adopted run's crash must not reset on pre-adoption time: \
         adopted at +{:?} (epsilon {adoption_epsilon:?}), events {events:#?}",
        adopted_at.saturating_duration_since(began)
    );
    wait_until_state(
        &facade_b,
        "adopted-crasher",
        |s| s == LifecycleState::Running,
        Duration::from_secs(25),
        "the post-adoption relaunch (run 3) to land running",
    );
    let status = facade_b.instance_status("adopted-crasher").unwrap();
    assert_eq!(
        status.restart_count, 2,
        "the adopted crash charged WITHOUT a reset (a mutant crediting \
         pre-adoption time reads 1)"
    );

    // B holds run 3 (adopted, disarmed kill-on-drop): stop it explicitly so
    // the drill leaves no orphan behind.
    facade_b
        .stop("adopted-crasher", Some(Duration::from_secs(5)))
        .unwrap();
}
