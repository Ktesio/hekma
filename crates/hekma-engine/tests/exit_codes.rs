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
//!
//! The 18-3 review-round-1 killers, and the tests that carry them:
//! * the requested path must neither charge nor RESET the counter history →
//!   `restart_requested_exit_relaunches_without_charging_the_counter` (the
//!   7,7,75,75 shape; the surfaced `requested-recycle N` depth rides it)
//! * the reset must fire on the LIVENESS-expiry charged leg and be NAMED in
//!   the persisted cause →
//!   `a_liveness_expiry_after_a_stable_run_resets_then_charges_and_names_the_reset`
//! * the reset naming rides the PERSISTED cause, never the Unresponsive
//!   event → `the_reset_rides_the_persisted_cause_not_the_unresponsive_event`
//! * unresolvable adopted classes degrade loudly and classify nothing →
//!   `adopted_instance_with_unresolvable_exit_codes_classifies_nothing`

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hekma_engine::{
    AdapterRef, DiagnosticSink, Engine, EngineOptions, LifecycleState, RestartPolicy,
    TransitionCause,
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
    // MUTATION KILLER (a) — 18-3 review round 1 (B3) shape: the
    // restart-requested path runs the crash-path dance (terminal failed +
    // relaunch, backoff verbatim) but must NEVER touch the
    // consecutive-failure counter — neither CHARGE it nor RESET it. The
    // drill first builds REAL counter history (two charged code-7 crashes
    // via `--crash-times 2`: the count reads 1, then 2), then runs TWO
    // exit-75 recycles: the count must STILL read 2 after each. A
    // charging mutant reads 3, then 4; a ZEROING mutant
    // (`set_restart_count(name, 0)` on the requested path) reads 0 — both
    // die, and counter CONTINUITY across the requested path is pinned
    // (a requested recycle is not the operator's fresh start).
    //
    // Shape note (18-3 review deviation, recorded in the spec change
    // log): a true 7→75→7 interleave is IMPOSSIBLE with the fixture —
    // launch args are snapshotted at registration, so a manifest rewrite
    // cannot change a chain's relaunch args, and `--crash-times` bounds
    // crashes to the FIRST N launches. The honest sequence is therefore
    // 7, 7, 75, 75 — the charged history first, the recycles after —
    // which is exactly the order the zeroing mutant is exposed to.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let crash_state = manifest.path().join("crash-count");
    write_manifest(
        manifest.path(),
        "relauncher",
        &[
            "--crash-after-ms",
            "900",
            "--crash-with",
            "7",
            "--crash-times",
            "2",
            "--crash-state",
            &crash_state.to_string_lossy(),
            "--exit-with",
            "75:2500",
            "--linger-ms",
            "600000",
        ],
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

    // Crash 1 (code 7, young — 900ms ≪ the production window): charged
    // failure #1, verbatim crash cause, relaunch on the base 1s leg.
    wait_until_state(
        &facade,
        "relauncher",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(15),
        "the first code-7 crash to land failed",
    );
    let events = facade.transition_events("relauncher").unwrap();
    let failed = events
        .iter()
        .rev()
        .find(|e| e.new_state == LifecycleState::Failed)
        .expect("a failed transition");
    match &failed.cause {
        TransitionCause::Crashed { detail } => {
            assert_eq!(
                detail.as_str(),
                "process exited unexpectedly with code 7",
                "the young charged crash carries the verbatim detail"
            );
        }
        other => panic!("expected a crashed cause, got {other:?}"),
    }
    wait_until_state(
        &facade,
        "relauncher",
        |s| s == LifecycleState::Running,
        Duration::from_secs(20),
        "relaunch 1 to land running (the base leg)",
    );
    let count = facade.instance_status("relauncher").unwrap().restart_count;
    assert_eq!(count, 1, "crash 1 charged failure #1");

    // Crash 2 (code 7, young): charged failure #2, relaunch on the 2s leg.
    wait_until_state(
        &facade,
        "relauncher",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(15),
        "the second code-7 crash to land failed",
    );
    wait_until_state(
        &facade,
        "relauncher",
        |s| s == LifecycleState::Running,
        Duration::from_secs(25),
        "relaunch 2 to land running (the 2s leg)",
    );
    let count = facade.instance_status("relauncher").unwrap().restart_count;
    assert_eq!(count, 2, "crash 2 charged failure #2 — the history is real");

    // Recycle 1 (exit 75 — the crash bound is spent): the counter must be
    // UNTOUCHED. The cause names the declared class AND the surfaced
    // recycle depth (the streak was dropped by the charged crashes, so
    // this chain starts at depth 1).
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
            assert!(
                detail.contains("requested-recycle 1"),
                "the surfaced depth starts the post-crash chain at 1: {detail}"
            );
        }
        other => panic!("expected a restart-requested cause, got {other:?}"),
    }
    wait_until_state(
        &facade,
        "relauncher",
        |s| s == LifecycleState::Running,
        Duration::from_secs(25),
        "recycle 1 to land running (delay index 2+0+1 → the 4s leg)",
    );
    let count = facade.instance_status("relauncher").unwrap().restart_count;
    assert_eq!(
        count, 2,
        "recycle 1 must neither charge (3) nor reset (0) the counter"
    );

    // Recycle 2: same drill — the count is STILL 2, and the depth grew to
    // 2 (the chain paces; the counter does not).
    wait_until_state(
        &facade,
        "relauncher",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(15),
        "the second exit-75 to land failed",
    );
    let events = facade.transition_events("relauncher").unwrap();
    let failed = events
        .iter()
        .rev()
        .find(|e| e.new_state == LifecycleState::Failed)
        .expect("a failed transition");
    match &failed.cause {
        TransitionCause::RestartRequested { code, detail } => {
            assert_eq!(*code, 75);
            let detail = detail.as_deref().unwrap_or_default();
            assert!(
                detail.contains("requested-recycle 2"),
                "the second recycle carries depth 2: {detail}"
            );
        }
        other => panic!("expected a restart-requested cause, got {other:?}"),
    }
    wait_until_state(
        &facade,
        "relauncher",
        |s| s == LifecycleState::Running,
        Duration::from_secs(30),
        "recycle 2 to land running (delay index 2+1+1 → the 8s leg)",
    );
    let count = facade.instance_status("relauncher").unwrap().restart_count;
    assert_eq!(
        count, 2,
        "recycle 2 must neither charge nor reset the counter either"
    );

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
    // MUTATION KILLER (d) — 18-3 review round 1 (B4) shape: the stability
    // anchor RESTAMPS at every resume, and only the POST-RESUME run counts.
    // Run 1 is the POSITIVE CONTROL: it holds `running` ~14.7s (the 15s
    // deadline less start slop) ≥ the 13s window, so its crash
    // resets-then-charges — count 1, the reset NAMED in the cause. Run 2
    // pauses as soon as its relaunch lands Running (inside the first
    // second), holds 3.5s, resumes, and crashes on its own wall-clock 15s
    // deadline: the post-resume run is ~9.5-11.5s — UNDER the window — so
    // the crash charges WITHOUT a reset (count 2, no reset text in the
    // cause). A mutant that kept the pre-pause anchor, or let the pause
    // interval count toward stability, reads run 2 as ≥ 13s "stable" and
    // resets (count 1) — dead on run 2; a mutant that never resets dies on
    // run 1. The crash deadline (15s) comfortably outlives the pause hold
    // (~4.5s), so the post-resume crash lands `running → failed` on every
    // OS — one uniform assertion, no per-OS split.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_manifest(
        manifest.path(),
        "frozen-crasher",
        &[
            "--crash-after-ms",
            "15000",
            "--crash-with",
            "7",
            "--linger-ms",
            "600000",
        ],
        Some("restart_requested = [75]"),
        true,
    );

    let engine = open_with_window(&state, 13_000);
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

    // Run 1: the positive control — a genuinely stable crash resets, then
    // charges as #1, and the reset is NAMED.
    wait_until_state(
        &facade,
        "frozen-crasher",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(30),
        "run 1's stable crash to land failed",
    );
    let events = facade.transition_events("frozen-crasher").unwrap();
    let failed = events
        .iter()
        .rev()
        .find(|e| e.new_state == LifecycleState::Failed)
        .expect("a failed transition");
    match &failed.cause {
        TransitionCause::Crashed { detail } => {
            assert!(
                detail.starts_with("process exited unexpectedly with code 7"),
                "run 1: the detail leads with the exit: {detail}"
            );
            assert!(
                detail.contains("the previous run was stable for")
                    && detail.contains("consecutive-failure count reset"),
                "run 1 (positive control): the reset is NAMED: {detail}"
            );
        }
        other => panic!("expected a crashed cause, got {other:?}"),
    }
    wait_until_state(
        &facade,
        "frozen-crasher",
        |s| s == LifecycleState::Running,
        Duration::from_secs(20),
        "run 2 to land running",
    );
    let status = facade.instance_status("frozen-crasher").unwrap();
    assert_eq!(status.restart_count, 1, "run 1 reset-then-charged to 1");

    // Pause as soon as run 2 is up — the younger the better (the paused
    // interval must never leak into the window, and the 15s deadline must
    // outlive the hold). Poll until the row settles Paused; never assume
    // the transition landed just because the call returned.
    let paused = facade.pause("frozen-crasher").unwrap();
    assert_eq!(paused.state, LifecycleState::Paused);
    wait_until_state(
        &facade,
        "frozen-crasher",
        |s| s == LifecycleState::Paused,
        Duration::from_secs(10),
        "the pause to settle",
    );
    std::thread::sleep(Duration::from_millis(3_500));
    facade.resume("frozen-crasher").unwrap();

    // Run 2 crashes on its wall-clock deadline, ~9.5-11.5s into the
    // POST-RESUME run — under the 13s window: charged WITHOUT a reset.
    wait_until_state(
        &facade,
        "frozen-crasher",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(30),
        "run 2's post-resume crash to land failed",
    );
    let events = facade.transition_events("frozen-crasher").unwrap();
    let failed = events
        .iter()
        .rev()
        .find(|e| e.new_state == LifecycleState::Failed)
        .expect("a failed transition");
    match &failed.cause {
        TransitionCause::Crashed { detail } => {
            assert_eq!(
                detail.as_str(),
                "process exited unexpectedly with code 7",
                "run 2: the crash charges VERBATIM — a pause interval buys no reset"
            );
        }
        other => panic!("expected a crashed cause, got {other:?}"),
    }
    wait_until_state(
        &facade,
        "frozen-crasher",
        |s| s == LifecycleState::Running,
        Duration::from_secs(25),
        "run 3 to land running",
    );
    let status = facade.instance_status("frozen-crasher").unwrap();
    assert_eq!(
        status.restart_count, 2,
        "the post-pause crash charged WITHOUT a reset (an anchor that \
         counted the pause — or kept the pre-pause stamp — would have \
         reset to 1)"
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

/// A `Write` adapter delegating into a shared buffer (the liveness.rs /
/// story-10-2 sink shape): the test reads the sink's captured bytes while
/// the engine owns the boxed writer.
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

/// Poll the captured sink until a line matching `needle` appears, bounded.
/// Returns the line (the wording assertions read it).
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

/// `write_manifest` with a raw `[liveness]` section (the declared
/// heartbeat check the liveness-leg drills need): the local helper above
/// has no `.liveness()` passthrough, but the `ManifestFixture` builder
/// does — this composes the same defaults plus the section.
fn write_manifest_with_liveness(
    dir: &Path,
    kind: &str,
    args: &[&str],
    liveness_body: &str,
    exit_codes_body: Option<&str>,
) {
    let fixture = hekma_conformance::test_support::ManifestFixture::new(kind, args)
        .guaranteed_on_all_oses("interaction")
        .liveness(liveness_body);
    let fixture = match exit_codes_body {
        Some(body) => fixture.exit_codes(body),
        None => fixture,
    };
    fixture.write(dir);
}

/// The B1/B9 fixture: a heartbeat subject whose FIRST launch crashes young
/// (the `--crash-times 1` bound) and whose later launches beat
/// `hb.marker` every 100ms until 6s, then stop — the deterministic
/// unresponsive-expiry subject for the stable-window reset.
fn write_heartbeat_crasher(dir: &Path, kind: &str) {
    let crash_state = dir.join("crash-count");
    write_manifest_with_liveness(
        dir,
        kind,
        &[
            "--heartbeat-file",
            "hb.marker:100",
            "--heartbeat-stop-after",
            "6000",
            "--crash-after-ms",
            "1200",
            "--crash-with",
            "7",
            "--crash-times",
            "1",
            "--crash-state",
            &crash_state.to_string_lossy(),
            "--linger-ms",
            "600000",
        ],
        "[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = 600",
        None,
    );
}

#[test]
fn a_liveness_expiry_after_a_stable_run_resets_then_charges_and_names_the_reset() {
    // MUTATION KILLER (B1) — the unresponsive-expiry leg of the stable
    // reset: a charged crash need not be a process EXIT. Run 1 crashes
    // young (1.2s < the 2s window) → count 1. Run 2 (the crash bound is
    // spent) beats its heartbeat until 6s and then stops: the declared
    // check expires it ~600ms later, after a ~6.6s run ≥ the window — so
    // the expiry RESETS the streak and charges as #1: the count reads 1
    // AND the persisted failed-cause names the reset (the naming rides
    // the ONE `set_restart_count` write; see
    // `the_reset_rides_the_persisted_cause_not_the_unresponsive_event`
    // for the event-cause boundary). A mutant that skips the anchor on
    // the liveness path reads 2; a mutant that drops the cause from the
    // write reads a bare cause — dead either way.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_heartbeat_crasher(manifest.path(), "stable-hb");

    let engine = open_with_window(&state, 2_000);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "stable-hb",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("stable-hb", RestartPolicy::OnFailure)
        .unwrap();

    let started = facade.start("stable-hb").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // Run 1: the young crash charges #1 — no reset (1.2s < 2s).
    wait_until_state(
        &facade,
        "stable-hb",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(15),
        "run 1's young crash to land failed",
    );
    let status = facade.instance_status("stable-hb").unwrap();
    assert_eq!(status.restart_count, 1, "run 1 charged young");

    wait_until_state(
        &facade,
        "stable-hb",
        |s| s == LifecycleState::Running,
        Duration::from_secs(20),
        "run 2 to land running",
    );

    // Run 2: heartbeat stops at its +6s; the check expires it ~600ms
    // later — a ~6.6s run, comfortably over the 2s window. Read the
    // Failed landing FAST: the reset-then-charge relaunch leg is the
    // base 1s, and the persisted cause is readable while the row sits
    // Failed (the ~1s window; the 250ms reaper tick leaves ~0.7s).
    wait_until_state(
        &facade,
        "stable-hb",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(30),
        "run 2's unresponsive expiry to land failed",
    );
    let status = facade.instance_status("stable-hb").unwrap();
    assert_eq!(
        status.restart_count, 1,
        "the stable expiry reset-then-charged to 1 (a mutant skipping \
         the anchor on the liveness path reads 2)"
    );
    assert!(
        status
            .failed_cause
            .as_deref()
            .is_some_and(|c| c.contains("consecutive-failure count reset")),
        "the persisted cause names the reset: {:?}",
        status.failed_cause
    );

    wait_until_state(
        &facade,
        "stable-hb",
        |s| s == LifecycleState::Running,
        Duration::from_secs(20),
        "run 3 to land running",
    );
    facade
        .stop("stable-hb", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn the_reset_rides_the_persisted_cause_not_the_unresponsive_event() {
    // MUTATION KILLER (B9) — the SURFACE boundary of the reset naming:
    // the persisted failed-cause (last_known_cause, carried by the ONE
    // `set_restart_count` write) carries the reset text, but the
    // Unresponsive EVENT cause stays check + streak + onset/verdict —
    // stuffing the reset into the event would double-render it in
    // `show` and garble the transition line. Same drill as the B1
    // killer; the assert here is on the EVENT: the last Failed cause is
    // `Unresponsive` and its detail names NO reset — while the persisted
    // cause, read in the same ~1s Failed window, does.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_heartbeat_crasher(manifest.path(), "stable-hb2");

    let engine = open_with_window(&state, 2_000);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "stable-hb2",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("stable-hb2", RestartPolicy::OnFailure)
        .unwrap();

    let started = facade.start("stable-hb2").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // Run 1: the young crash (a plain Crashed event — no Unresponsive
    // event may exist yet).
    wait_until_state(
        &facade,
        "stable-hb2",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(15),
        "run 1's young crash to land failed",
    );
    let events = facade.transition_events("stable-hb2").unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.cause, TransitionCause::Unresponsive { .. })),
        "the young crash is a Crashed event, not an expiry: {events:#?}"
    );
    wait_until_state(
        &facade,
        "stable-hb2",
        |s| s == LifecycleState::Running,
        Duration::from_secs(20),
        "run 2 to land running",
    );

    // Run 2's expiry lands Failed. The event must be `Unresponsive` with
    // NO reset text in its detail; the persisted cause must carry it.
    wait_until_state(
        &facade,
        "stable-hb2",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(30),
        "run 2's unresponsive expiry to land failed",
    );
    let events = facade.transition_events("stable-hb2").unwrap();
    let failed = events
        .iter()
        .rev()
        .find(|e| e.new_state == LifecycleState::Failed)
        .expect("a failed transition");
    match &failed.cause {
        TransitionCause::Unresponsive {
            check,
            failing_for_ms,
            detail,
        } => {
            assert!(
                check.contains("heartbeat"),
                "the event names the declared check: {check}"
            );
            assert!(
                *failing_for_ms >= 600,
                "the streak is the crossed window: {failing_for_ms}ms"
            );
            assert!(
                !detail
                    .as_deref()
                    .is_some_and(|d| d.contains("consecutive-failure count reset")),
                "the reset naming must NOT ride the Unresponsive event: {detail:?}"
            );
        }
        other => panic!("expected an unresponsive cause, got {other:?}"),
    }
    let status = facade.instance_status("stable-hb2").unwrap();
    assert!(
        status
            .failed_cause
            .as_deref()
            .is_some_and(|c| c.contains("consecutive-failure count reset")),
        "the PERSISTED cause is where the reset naming rides: {:?}",
        status.failed_cause
    );

    wait_until_state(
        &facade,
        "stable-hb2",
        |s| s == LifecycleState::Running,
        Duration::from_secs(20),
        "run 3 to land running",
    );
    facade
        .stop("stable-hb2", Some(Duration::from_secs(5)))
        .unwrap();
}

#[test]
fn adopted_instance_with_unresolvable_exit_codes_classifies_nothing() {
    // MUTATION KILLER (B2) — the adoption-time RE-RESOLUTION of the
    // declared exit-code classes degrades HONESTLY (the liveness twin's
    // posture): engine A registers + starts a DETACHED instance under a
    // valid `restart_requested = [75]`; A drops; the manifest's section
    // drifts INVALID (an overlap — 75 declared in BOTH lists, which
    // `ExitCodes::classes` refuses); engine B adopts. The re-resolution
    // FAILS: exactly ONE diagnostic names it, the adoption still
    // succeeds (Running), and the adopted exit-75 lands a PLAIN charged
    // crash — NOT a restart-requested recycle (the classes are simply
    // not supervised this Run). A mutant that adopted the stale
    // registration classes (or degraded silently) would recycle on the
    // declared code — dead.
    //
    // OS shape note (no new Windows CI claimed): on UNIX the adopted
    // exit's code is additionally unavailable BY STRUCTURE (the process
    // is not this engine's child), so the no-classification pin rides
    // the codeless-exit gate there; on WINDOWS the adopted poll reads
    // the real code (GetExitCodeProcess), and it is the `None` classes —
    // the resolution failure this test pins — that classifies nothing.
    // Both roads lead to the same plain-crash assert.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    let args = ["--exit-with", "75:9000", "--linger-ms", "600000"];
    write_manifest(
        manifest.path(),
        "adopted-exits",
        &args,
        Some("restart_requested = [75]"),
        false,
    );

    {
        let engine_a = open(&state);
        let facade_a = engine_a.blocking();
        facade_a
            .register_with_adapter(
                "adopted-exits",
                &AdapterRef::Manifest(manifest.path().to_path_buf()),
            )
            .unwrap();
        facade_a
            .set_restart_policy("adopted-exits", RestartPolicy::OnFailure)
            .unwrap();
        let started = facade_a.start_detached("adopted-exits").unwrap();
        assert_eq!(started.state, LifecycleState::Running);
        std::thread::sleep(Duration::from_millis(500));
    } // engine A drops; the DETACHED agent survives.

    // The section drifts INVALID under the dropped engine: 75 in BOTH
    // lists — an overlap the resolver refuses. (The launch args were
    // snapshotted at registration; the rewrite cannot change them.)
    write_manifest(
        manifest.path(),
        "adopted-exits",
        &args,
        Some("restart_requested = [75]\nclean = [75]"),
        false,
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(&shared)))));
    let engine_b = Engine::open_with_diagnostics(Some(state.path().to_path_buf()), sink)
        .expect("open engine B");
    let facade_b = engine_b.blocking();

    let line = wait_for_diagnostic(
        &shared,
        "could not be resolved",
        Duration::from_secs(10),
        "the degraded exit-codes resolution diagnostic",
    );
    assert!(
        line.contains("NOT classified this Run"),
        "the diagnostic names the degradation and its scope: {line}"
    );
    assert_eq!(
        diagnostic_count(&shared, "NOT classified this Run"),
        1,
        "exactly ONE degraded-resolution diagnostic"
    );

    let status = facade_b.instance_status("adopted-exits").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Running,
        "the adoption succeeds despite the unresolvable classes"
    );

    // Restore a VALID section now: the adopted Run's classes were resolved
    // ONCE (at adoption) and stand degraded — the rewrite cannot re-arm
    // them. But the section is RE-CHECKED at every start, so leaving the
    // overlap in place would refuse the policy relaunch's own start too;
    // the drift has served its purpose by the time the diagnostic landed.
    write_manifest(
        manifest.path(),
        "adopted-exits",
        &args,
        Some("restart_requested = [75]"),
        false,
    );

    // The adopted exit-75 (at its +9s) is a PLAIN charged crash — no
    // classified cause, no requested recycle.
    wait_until_state(
        &facade_b,
        "adopted-exits",
        |s| s == LifecycleState::Failed,
        Duration::from_secs(25),
        "the adopted exit-75 to land failed",
    );
    let events = facade_b.transition_events("adopted-exits").unwrap();
    let failed = events
        .iter()
        .rev()
        .find(|e| e.new_state == LifecycleState::Failed)
        .expect("a failed transition");
    match &failed.cause {
        TransitionCause::Crashed { detail } => {
            assert!(
                detail.starts_with("process exited unexpectedly"),
                "an adopted exit with unresolvable classes is a PLAIN crash \
                 (no classified cause): {detail}"
            );
        }
        other => panic!("expected an unclassified crashed cause, got {other:?}"),
    }
    let status = facade_b.instance_status("adopted-exits").unwrap();
    assert_eq!(
        status.restart_count, 1,
        "the unclassified adopted exit CHARGES (the requested path would not)"
    );

    wait_until_state(
        &facade_b,
        "adopted-exits",
        |s| s == LifecycleState::Running,
        Duration::from_secs(20),
        "the policy relaunch to land running",
    );
    facade_b
        .stop("adopted-exits", Some(Duration::from_secs(5)))
        .unwrap();
}
