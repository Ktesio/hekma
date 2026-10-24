//! Integration tests for story 18-4 — COOPERATIVE pause commands and the
//! DRAIN verb — driven through the engine's PUBLIC blocking facade only
//! (spine AD-2/AD-13), spawning the REAL `fake_agent` helper.
//!
//! Every test names the mutation it kills (the spec's mutation-killer matrix,
//! (a)–(j)):
//!
//! * (a) `cooperative_pause_runs_the_declared_command_not_the_signal` — the
//!   cooperative-FIRST dispatch: a declared `[lifecycle.pause]` command
//!   outranks the signal path even under a `guaranteed` capability (the
//!   signal-dispatch mutant freezes the heartbeat; the command path does not).
//! * (b) `cooperative_pause_stops_the_work_and_resume_runs_the_command` —
//!   the commands' observable side effects: the agent's own work stops while
//!   paused and resumes after; the causes are `pause-cooperative` /
//!   `resume-cooperative` naming the rendered command.
//! * (c) (same test) the post-resume stop never claims a cooperative skip —
//!   the mutant that reads the cooperative flag outside the paused-state gate
//!   would emit the skip note on a RUNNING instance's stop.
//! * (d1) `pause_command_failure_rejects_with_state_untouched` (nonzero exit),
//!   (d2) (spawn failure), (d3) `pause_command_overrun_is_killed_by_the_cap`
//!   (the standalone cap seam is real), (d4)
//!   `zero_cap_override_is_a_no_op` (the zero-guard keeps the production cap).
//! * (e) `no_template_manifest_keeps_the_signal_pause` — byte-identity of the
//!   pre-18-4 surface for manifests WITHOUT lifecycle commands (Unix
//!   data-driven skip; NO `#[cfg]`).
//! * (f) `drain_completes_when_the_idle_signal_arrives_mid_wait` — the wait
//!   loop genuinely waits (the skip-the-wait mutant reports ~0 waited ms).
//! * (g) `drain_that_outlives_its_bound_still_stops` — the unconditional last
//!   step and the honest `completed: false`.
//! * (h) `degenerate_drain_stops_directly_and_says_so` — no command, no
//!   signal: stop directly, `completed: true, waited_ms: 0`, and SAY so.
//! * (i) `pair_without_drain_section_drains_under_the_default_bound` — the
//!   default bound is a CEILING, and the stop ladder skips the OS resume
//!   signal for a cooperative pause (the honest skip note).
//! * (j) `liveness_skip_holds_for_a_cooperative_pause` — a stale heartbeat
//!   must not fail a cooperatively-paused row.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hekma_engine::{
    AdapterRef, Blocking, DiagnosticSink, Engine, EngineError, EngineOptions, LifecycleState, OsId,
    RestartPolicy,
};
use tempfile::TempDir;

/// The bounded budget for every committed-state / file-arrival poll in this
/// suite: generous THROUGHPUT margin for a real subprocess round trip on a
/// slow CI runner, never a race window (the `tests/acp_lifecycle.rs`
/// POLL_BUDGET posture).
const POLL_BUDGET: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Helpers (the pause.rs / liveness.rs / acp_lifecycle.rs conventions)
// ---------------------------------------------------------------------------

/// The current-OS key for a manifest `[capabilities.pause]` table, as the wire
/// string the manifest uses (`linux`/`macos`/`windows`). Runtime data, not cfg.
fn current_os_key() -> &'static str {
    match OsId::current() {
        OsId::Linux => "linux",
        OsId::Macos => "macos",
        OsId::Windows => "windows",
        OsId::Other => "other",
    }
}

/// Write a manifest with the story-18-4 lifecycle shape: `[lifecycle.start]`
/// pointed at `fake_agent` with `agent_args`, OPTIONAL `[lifecycle.pause]` /
/// `[lifecycle.resume]` command sub-tables (each `fake_agent` with the given
/// args — in practice a `--touch` fixture; the pair rule requires BOTH or
/// NEITHER), an OPTIONAL raw `[drain]` section body, and an OPTIONAL
/// current-OS pause capability level (with interaction guaranteed everywhere
/// so registration passes). `contract_version` is pinned to `"1.1.0"` — the
/// contract version that introduced `[drain]` (same-major negotiation accepts
/// it; if 1.1 support is ever dropped this file fails loudly).
fn write_drain_manifest(
    dir: &Path,
    kind: &str,
    agent_args: &[&str],
    pause_args: Option<&[&str]>,
    resume_args: Option<&[&str]>,
    drain_body: Option<&str>,
    pause_level_current_os: Option<&str>,
) {
    let bin = hekma_conformance::fake_agent_bin();
    let args_toml = |args: &[&str]| {
        args.iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let pause = pause_args
        .map(|args| {
            format!(
                "\n[lifecycle.pause]\nexec = {bin:?}\nargs = [{}]",
                args_toml(args)
            )
        })
        .unwrap_or_default();
    let resume = resume_args
        .map(|args| {
            format!(
                "\n[lifecycle.resume]\nexec = {bin:?}\nargs = [{}]",
                args_toml(args)
            )
        })
        .unwrap_or_default();
    let interaction = "\n[capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\nwindows = \"guaranteed\"\n";
    let capability = match pause_level_current_os {
        Some(level) => format!(
            "{interaction}\n[capabilities.pause]\n{os} = \"{level}\"\n",
            os = current_os_key(),
        ),
        None => interaction.to_string(),
    };
    let drain = drain_body
        .map(|body| format!("\n[drain]\n{body}\n"))
        .unwrap_or_default();
    let body = format!(
        "\ncontract_version = \"1.1.0\"\n\n[adapter]\nkind = \"{kind}\"\n\n\
         [lifecycle.start]\nexec = {bin:?}\nargs = [{agent_toml}]\n{pause}{resume}{capability}{drain}\n\
         [metering]\nsource = \"self-reported\"\n",
        bin = bin.to_string_lossy(),
        kind = kind,
        agent_toml = args_toml(agent_args),
        pause = pause,
        resume = resume,
        capability = capability,
        drain = drain,
    );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

/// A `Write` adapter delegating into a shared buffer (the diagnostic-sink
/// suite's capture shape): the test reads the sink's bytes while the engine
/// owns the boxed writer.
#[derive(Clone)]
struct SharedCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedCapture {
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

/// Open an engine over a fresh temp state dir.
fn open(base: &TempDir) -> Engine {
    Engine::open(Some(base.path().to_path_buf())).expect("open engine")
}

/// Open an engine with the diagnostic sink installed FROM OPEN (the airtight
/// install — no diagnostic can slip to stderr before the sink is in place).
fn open_with_sink(base: &TempDir, shared: &Arc<Mutex<Vec<u8>>>) -> Engine {
    Engine::open_with_diagnostics(Some(base.path().to_path_buf()), make_sink(shared))
        .expect("open engine with diagnostics")
}

/// Open an engine with explicit options (the cap-seam tests).
fn open_with_options(base: &TempDir, options: EngineOptions) -> Engine {
    Engine::open_with_options(Some(base.path().to_path_buf()), options)
        .expect("open engine with options")
}

/// Register `name` against the manifest in `manifest` and start it, asserting
/// the running landing (the shared first leg of nearly every test).
fn register_and_start(facade: &Blocking, manifest: &TempDir, name: &str) {
    facade
        .register_with_adapter(name, &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let started = facade.start(name).unwrap();
    assert_eq!(started.state, LifecycleState::Running);
}

/// The instance's Agent Home (`<state>/agents/<name>`) — the working directory
/// of BOTH the supervised agent and the lifecycle commands, so every relative
/// fixture path in these tests lands here.
fn agent_home(base: &Path, name: &str) -> PathBuf {
    base.join("agents").join(name)
}

/// The sentinel file a lifecycle command's `--touch` creates (relative to the
/// Agent Home).
fn sentinel(base: &Path, name: &str, file: &str) -> PathBuf {
    agent_home(base, name).join(file)
}

/// The instance's captured agent stdout log (`<home>/logs/agent.log`).
fn agent_log(base: &Path, name: &str) -> PathBuf {
    agent_home(base, name).join("logs").join("agent.log")
}

/// Count `heartbeat <n>` lines in the agent's captured output log (the
/// pause.rs helper).
fn heartbeat_lines(log: &Path) -> usize {
    std::fs::read_to_string(log)
        .map(|c| c.lines().filter(|l| l.starts_with("heartbeat ")).count())
        .unwrap_or(0)
}

/// Count `work <n>` lines in the `--watch-sentinel` work log.
fn work_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|c| c.lines().filter(|l| l.starts_with("work ")).count())
        .unwrap_or(0)
}

/// The sink's captured text.
fn sink_text(shared: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(shared.lock().unwrap().clone()).expect("utf-8 sink")
}

/// Poll `cond` until it holds or the budget burns (throughput margin, not a
/// race window — see [`POLL_BUDGET`]).
fn wait_for(budget: Duration, what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + budget;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The rendered `[lifecycle.pause]` command as the causes record it: the
/// fake_agent exec + the args, space-joined.
fn rendered_command(args: &[&str]) -> String {
    let mut rendered = hekma_conformance::fake_agent_bin()
        .to_string_lossy()
        .into_owned();
    for arg in args {
        rendered.push(' ');
        rendered.push_str(arg);
    }
    rendered
}

/// The wire form of a command needle. The serialized cause escapes the
/// backslashes of a Windows bin path (`D:\a\…` → `D:\\a\\…`), so a raw-text
/// `contains` with unescaped separators can never match there. The
/// JSON-encoded string literal (quotes included) is byte-identical to the
/// wire segment on every platform.
fn wire_command(cmd: &str) -> String {
    serde_json::to_string(cmd).unwrap()
}

// ---------------------------------------------------------------------------
// (a) The cooperative-FIRST dispatch — the command outranks the signal path
// ---------------------------------------------------------------------------

#[test]
fn cooperative_pause_runs_the_declared_command_not_the_signal() {
    // KILLER (a): with a `guaranteed` pause capability AND a declared
    // `[lifecycle.pause]` command, the COMMAND path must own the pause. The
    // signal-dispatch mutant (running SIGSTOP instead of the command) is killed
    // three ways: the pause sentinel never appears (the command never ran), the
    // cause is a plain `command`/pause instead of `pause-cooperative`, and — on
    // Unix, where the signal is a real SIGSTOP — the heartbeat FREEZES under the
    // mutant but must keep growing here (a cooperative pause suspends the
    // agent's cooperation, never the process).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        &[
            "--heartbeat-ms",
            "50",
            "--linger-ms",
            "600000",
            "--watch-sentinel",
            "pause.sentinel:resume.sentinel:work.log",
        ],
        Some(&["--touch", "pause.sentinel"]),
        Some(&["--touch", "resume.sentinel"]),
        None,
        Some("guaranteed"),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    let log = agent_log(state.path(), "svc");
    wait_for(POLL_BUDGET, "the heartbeat to start", || {
        heartbeat_lines(&log) >= 2
    });

    let paused = facade.pause("svc").unwrap();
    assert_eq!(paused.state, LifecycleState::Paused);

    // The COMMAND ran (the sentinel exists; the resume sentinel does not — the
    // resume command was not invoked by a pause).
    let pause_sentinel = sentinel(state.path(), "svc", "pause.sentinel");
    wait_for(POLL_BUDGET, "the pause sentinel to land", || {
        pause_sentinel.exists()
    });
    assert!(
        !sentinel(state.path(), "svc", "resume.sentinel").exists(),
        "a pause must not run the resume command"
    );

    // The process was NOT suspended: the heartbeat keeps growing across a
    // window that would surface a SIGSTOP freeze many times over (the mutant's
    // frozen count cannot grow).
    let baseline = heartbeat_lines(&log);
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        heartbeat_lines(&log) > baseline,
        "a cooperative pause must NOT suspend the process (heartbeat {baseline} must grow)"
    );

    // The cause is the cooperative one, naming the rendered command.
    let events = facade.transition_events("svc").unwrap();
    let pause_evt = events
        .iter()
        .find(|e| e.new_state == LifecycleState::Paused)
        .expect("a running→paused edge");
    let cause = serde_json::to_string(&pause_evt.cause).unwrap();
    assert!(
        cause.contains("pause-cooperative"),
        "the declared command must win over the signal path (cause: {cause})"
    );
    let expected = rendered_command(&["--touch", "pause.sentinel"]);
    assert!(
        cause.contains(&wire_command(&expected)),
        "the cause must name the rendered command `{expected}` (cause: {cause})"
    );

    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
}

// ---------------------------------------------------------------------------
// (b) + (c) The commands' observable side effects — work stops, work resumes,
// the honest stop surface after a resume
// ---------------------------------------------------------------------------

#[test]
fn cooperative_pause_stops_the_work_and_resume_runs_the_command() {
    // KILLER (b): the pause command's side effect must be OBSERVABLE in the
    // agent's own work — the `--watch-sentinel` work log stops growing while
    // paused (the agent honors its pause) and resumes after. The mutant that
    // commits the transition WITHOUT running the command leaves the work
    // growing (and the sentinel missing).
    // KILLER (c): the post-resume stop must NOT claim a cooperative skip — the
    // mutant that reads the cooperative flag outside the paused-state gate
    // emits the "paused COOPERATIVELY" note on a RUNNING instance's stop (a
    // lie: the instance is not even paused).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        &[
            "--linger-ms",
            "600000",
            "--watch-sentinel",
            "pause.sentinel:resume.sentinel:work.log",
        ],
        Some(&["--touch", "pause.sentinel"]),
        Some(&["--touch", "resume.sentinel"]),
        None,
        Some("guaranteed"),
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let engine = open_with_sink(&state, &shared);
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    let work = sentinel(state.path(), "svc", "work.log");
    wait_for(POLL_BUDGET, "the work log to start", || {
        work_lines(&work) >= 2
    });

    // Pause: the work STOPS (the agent honors its pause command's sentinel).
    facade.pause("svc").unwrap();
    std::thread::sleep(Duration::from_millis(200)); // let the 25ms watcher notice
    let baseline = work_lines(&work);
    let watch_until = Instant::now() + Duration::from_millis(700);
    while Instant::now() < watch_until {
        let now = work_lines(&work);
        assert!(
            now <= baseline,
            "work must NOT grow while paused: baseline {baseline}, saw {now}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Resume: the resume COMMAND runs (the sentinel lands) and the work grows
    // again.
    let resumed = facade.resume("svc").unwrap();
    assert_eq!(resumed.state, LifecycleState::Running);
    let resume_sentinel = sentinel(state.path(), "svc", "resume.sentinel");
    wait_for(POLL_BUDGET, "the resume sentinel to land", || {
        resume_sentinel.exists()
    });
    let after = work_lines(&work);
    wait_for(POLL_BUDGET, "the work to resume", || {
        work_lines(&work) > after
    });

    // The causes are the cooperative pair, naming the rendered commands.
    let events = facade.transition_events("svc").unwrap();
    let pause_evt = events
        .iter()
        .find(|e| e.new_state == LifecycleState::Paused)
        .expect("a running→paused edge");
    let pause_cause = serde_json::to_string(&pause_evt.cause).unwrap();
    assert!(
        pause_cause.contains("pause-cooperative")
            && pause_cause.contains(&wire_command(&rendered_command(&[
                "--touch",
                "pause.sentinel"
            ]))),
        "pause cause: {pause_cause}"
    );
    let resume_evt = events
        .iter()
        .find(|e| e.prior_state == LifecycleState::Paused && e.new_state == LifecycleState::Running)
        .expect("a paused→running edge");
    let resume_cause = serde_json::to_string(&resume_evt.cause).unwrap();
    assert!(
        resume_cause.contains("resume-cooperative")
            && resume_cause.contains(&wire_command(&rendered_command(&[
                "--touch",
                "resume.sentinel"
            ]))),
        "resume cause: {resume_cause}"
    );

    // KILLER (c): the post-resume stop of a RUNNING instance must not claim a
    // cooperative skip — the skip note is the paused-state ladder's voice, and
    // the instance is not paused.
    facade
        .stop("svc", Some(Duration::from_millis(500)))
        .unwrap();
    let captured = sink_text(&shared);
    assert!(
        !captured.contains("paused COOPERATIVELY"),
        "a stop after resume must not claim a cooperative pause (sink: {captured})"
    );
}

// ---------------------------------------------------------------------------
// (e) Byte-identity of the NO-template surface — the signal path unchanged
// ---------------------------------------------------------------------------

#[test]
fn no_template_manifest_keeps_the_signal_pause() {
    // KILLER (e): a manifest WITHOUT lifecycle commands must behave exactly as
    // pre-18-4: a guaranteed pause is a REAL suspension (the heartbeat freezes)
    // recorded with the plain command cause — never the cooperative cause, and
    // no lifecycle sentinel can exist (there is no command to run). Unix
    // data-driven skip (the guaranteed path is a real signal there); NO
    // `#[cfg]` (this file is outside the backends allowlist).
    if OsId::current() == OsId::Windows {
        return;
    }
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        &["--heartbeat-ms", "50", "--linger-ms", "600000"],
        None,
        None,
        None,
        Some("guaranteed"),
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let engine = open_with_sink(&state, &shared);
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    let log = agent_log(state.path(), "svc");
    wait_for(POLL_BUDGET, "the heartbeat to start", || {
        heartbeat_lines(&log) >= 2
    });

    let paused = facade.pause("svc").unwrap();
    assert_eq!(paused.state, LifecycleState::Paused);

    // No command ran — there is nothing to have created a sentinel.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !sentinel(state.path(), "svc", "pause.sentinel").exists(),
        "a no-template manifest has no pause command to run"
    );

    // The REAL suspension: the heartbeat freezes across a window a live
    // 50ms heartbeat would cross ~20 times (the pause.rs LOW-3 posture —
    // never-grow, not two instantaneous samples).
    let baseline = heartbeat_lines(&log);
    let watch_until = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < watch_until {
        let now = heartbeat_lines(&log);
        assert!(
            now <= baseline,
            "a signal pause must really suspend: baseline {baseline}, saw {now}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // The cause is the PLAIN command cause (byte-identical pre-18-4), never
    // the cooperative one.
    let events = facade.transition_events("svc").unwrap();
    let pause_evt = events
        .iter()
        .find(|e| e.new_state == LifecycleState::Paused)
        .expect("a running→paused edge");
    let cause = serde_json::to_string(&pause_evt.cause).unwrap();
    assert!(
        cause.contains("\"kind\":\"command\"") && cause.contains("pause"),
        "the no-template pause keeps the plain command cause: {cause}"
    );
    assert!(
        !cause.contains("cooperative"),
        "a no-template pause must not claim the cooperative cause: {cause}"
    );

    // Resume wakes it (SIGCONT), byte-identical.
    let resumed = facade.resume("svc").unwrap();
    assert_eq!(resumed.state, LifecycleState::Running);
    wait_for(POLL_BUDGET, "the heartbeat to resume", || {
        heartbeat_lines(&log) > baseline
    });

    // Review round 1 (B10), the OTHER stop-ladder arm with a signal pause
    // riding it: pause AGAIN, then stop — the ladder must RESUME the
    // suspended process before the graceful signal (a suspended process
    // cannot receive it) and SAY so. The cooperative-skip mutant (treating a
    // signal pause like the cooperative one) omits the resume and the note
    // never fires; the graceful signal would pend forever behind the
    // suspension.
    facade.pause("svc").unwrap();
    assert_eq!(
        facade.instance_status("svc").unwrap().instance.state,
        LifecycleState::Paused
    );
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
    assert_eq!(
        facade.instance_status("svc").unwrap().instance.state,
        LifecycleState::Stopped
    );
    let captured = sink_text(&shared);
    assert!(
        captured.contains("resumed the paused process before the stop ladder"),
        "a SIGNAL-paused instance's stop must resume the suspended process first, \
         and say so (sink: {captured})"
    );
}

// ---------------------------------------------------------------------------
// (d1) + (d2) A failing pause command rejects the verb — state untouched
// ---------------------------------------------------------------------------

#[test]
fn pause_command_failure_rejects_with_state_untouched() {
    // KILLER (d1)/(d2): a pause command that exits NONZERO (d1: `--exit-fast 3`)
    // or cannot even SPAWN (d2: a nonexistent exec) must REJECT the pause with
    // the typed `PauseCommandFailed` (naming the direction, the rendered
    // command, and the honest why) while the durable state stays `running` and
    // NO transition event lands — never a fake pause the stop ladder would
    // inherit. Both sub-killers in one test: the rejection contract is
    // identical for every failure shape.
    for (case, pause_args) in [
        ("nonzero exit", vec!["--exit-fast", "3"]),
        ("spawn failure", vec!["--definitely-not-a-real-flag-shape"]),
    ] {
        let state = TempDir::new().unwrap();
        let manifest = TempDir::new().unwrap();
        if case == "spawn failure" {
            // The spawn-failure shape needs a BOGUS EXEC for the pause
            // command, which the shared writer cannot express — hand-roll
            // this one manifest (same shape, exec pointed at a path that
            // does not exist).
            let bin = hekma_conformance::fake_agent_bin();
            let body = format!(
                "\ncontract_version = \"1.1.0\"\n\n[adapter]\nkind = \"svc\"\n\n\
                 [lifecycle.start]\nexec = {bin:?}\nargs = [\"--linger-ms\", \"600000\"]\n\n\
                 [lifecycle.pause]\nexec = \"./no-such-lifecycle-binary-anywhere\"\nargs = []\n\n\
                 [lifecycle.resume]\nexec = {bin:?}\nargs = [\"--touch\", \"resume.sentinel\"]\n\n\
                 [capabilities.interaction]\nlinux = \"guaranteed\"\nmacos = \"guaranteed\"\n\
                 windows = \"guaranteed\"\n\n[metering]\nsource = \"self-reported\"\n",
                bin = bin.to_string_lossy(),
            );
            std::fs::write(manifest.path().join("adapter.toml"), body).unwrap();
        } else {
            write_drain_manifest(
                manifest.path(),
                "svc",
                &["--linger-ms", "600000"],
                Some(&pause_args),
                Some(&["--touch", "resume.sentinel"]),
                None,
                Some("guaranteed"),
            );
        }

        let engine = open(&state);
        let facade = engine.blocking();
        register_and_start(&facade, &manifest, "svc");

        let result = facade.pause("svc");
        let Err(EngineError::PauseCommandFailed {
            name,
            direction,
            command,
            detail,
        }) = &result
        else {
            panic!(
                "a failing pause command must reject with PauseCommandFailed ({case}): {result:?}"
            );
        };
        assert_eq!(name, "svc");
        assert_eq!(direction, "pause");
        assert!(
            !command.is_empty(),
            "the rejection names the rendered command ({case})"
        );
        assert!(
            !detail.is_empty(),
            "the rejection carries the honest why ({case})"
        );

        // The durable state is UNTOUCHED — still running, no pause edge.
        let status = facade.instance_status("svc").unwrap();
        assert_eq!(
            status.instance.state,
            LifecycleState::Running,
            "a rejected pause leaves the instance running ({case})"
        );
        let events = facade.transition_events("svc").unwrap();
        assert!(
            !events.iter().any(|e| e.new_state == LifecycleState::Paused),
            "a rejected pause must not append a pause transition ({case})"
        );

        // The verb is retryable, not poison: a WELL-FORMED command after a
        // repaired manifest... (not staged here — the repair would need a
        // re-register; the point is the state stayed clean for one).
        facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
    }
}

// ---------------------------------------------------------------------------
// (d3) The standalone cap seam is REAL — an overrunning command is killed
// ---------------------------------------------------------------------------

#[test]
fn pause_command_overrun_is_killed_by_the_cap() {
    // KILLER (d3): the standalone pause-command cap (30s in production, the
    // `pause_command_cap_override` seam in tests) must be ENFORCED — a command
    // that hangs past it is killed and the pause REJECTS (never a fake pause),
    // and the kill lands BEFORE the command's side effect (the sentinel never
    // appears). The cap-is-advisory mutant (wait forever, or accept the
    // overrun) leaves the sentinel behind or returns success.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        &["--linger-ms", "600000"],
        // Touch then hang: the touch only lands if the cap FAILED to kill.
        Some(&["--touch", "capped.sentinel", "--hang-before", "60000"]),
        Some(&["--touch", "resume.sentinel"]),
        None,
        Some("guaranteed"),
    );

    let engine = open_with_options(
        &state,
        EngineOptions {
            pause_command_cap_override: Some(Duration::from_millis(300)),
            ..EngineOptions::default()
        },
    );
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    let result = facade.pause("svc");
    let Err(EngineError::PauseCommandFailed { detail, .. }) = &result else {
        panic!("an overrunning pause command must reject: {result:?}");
    };
    assert!(
        detail.contains("outlived") && detail.contains("bound"),
        "the rejection names the cap overrun: {detail}"
    );

    // The durable state is untouched and the sentinel NEVER landed (the kill
    // preceded the side effect).
    assert_eq!(
        facade.instance_status("svc").unwrap().instance.state,
        LifecycleState::Running
    );
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !sentinel(state.path(), "svc", "capped.sentinel").exists(),
        "a killed command must not have performed its side effect"
    );

    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
}

// ---------------------------------------------------------------------------
// (d4) The ZERO cap override is a no-op — the zero-guard keeps production cap
// ---------------------------------------------------------------------------

#[test]
fn zero_cap_override_is_a_no_op() {
    // KILLER (d4): `pause_command_cap_override: Some(ZERO)` must be IGNORED by
    // the zero-guard (the `stable_run_window_override` precedent) — the
    // production 30s cap stays. The mutant that APPLIES the zero cap rejects
    // every command instantly as an overrun; this pause command legitimately
    // needs ~150ms, so it must succeed.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        &["--linger-ms", "600000"],
        // 150ms of honest work before the touch — well inside the 30s
        // production cap, instant death under a zero cap.
        Some(&["--touch", "coop.sentinel", "--hang-before", "150"]),
        Some(&["--touch", "resume.sentinel"]),
        None,
        Some("guaranteed"),
    );

    let engine = open_with_options(
        &state,
        EngineOptions {
            pause_command_cap_override: Some(Duration::ZERO),
            ..EngineOptions::default()
        },
    );
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    let paused = facade.pause("svc").unwrap();
    assert_eq!(
        paused.state,
        LifecycleState::Paused,
        "the zero override must not shrink the production cap"
    );
    wait_for(POLL_BUDGET, "the sentinel to land", || {
        sentinel(state.path(), "svc", "coop.sentinel").exists()
    });

    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
}

// ---------------------------------------------------------------------------
// (d5) The cooperative arm outranks the CAPABILITY level — `unsupported`
// pauses cooperatively when the pair is declared
// ---------------------------------------------------------------------------

#[test]
fn unsupported_pause_level_still_pauses_cooperatively_under_a_pair() {
    // KILLER (d5, review round 1 B5): the COOPERATIVE-FIRST dispatch must own
    // the transition on COMMAND strength alone — a manifest declaring the
    // pair with `[capabilities.pause] unsupported` for the current OS still
    // pauses (the declared command IS the pause mechanism; there is no OS
    // suspension to be unsupported about). The capability-checking mutant
    // falls through to the level dispatch and fails fast with
    // CapabilityUnsupported, stranding the operator.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        &["--linger-ms", "600000"],
        Some(&["--touch", "pause.sentinel"]),
        Some(&["--touch", "resume.sentinel"]),
        None,
        Some("unsupported"),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    let paused = facade.pause("svc").unwrap();
    assert_eq!(
        paused.state,
        LifecycleState::Paused,
        "a declared pair pauses cooperatively even at capability level `unsupported`"
    );
    // The COMMAND ran (the sentinel exists) and the cause is the cooperative
    // one — never a capability refusal.
    wait_for(POLL_BUDGET, "the pause sentinel to land", || {
        sentinel(state.path(), "svc", "pause.sentinel").exists()
    });
    let events = facade.transition_events("svc").unwrap();
    let pause_evt = events
        .iter()
        .find(|e| e.new_state == LifecycleState::Paused)
        .expect("a running→paused edge");
    let cause = serde_json::to_string(&pause_evt.cause).unwrap();
    assert!(
        cause.contains("pause-cooperative"),
        "the unsupported-level pause records the cooperative cause: {cause}"
    );

    // Teardown: the stop ladder skips the OS resume (cooperative pause) and
    // stops.
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
}

// ---------------------------------------------------------------------------
// (f) The drain's wait genuinely waits — the idle signal lands MID-drain
// ---------------------------------------------------------------------------

#[test]
fn drain_completes_when_the_idle_signal_arrives_mid_wait() {
    // KILLER (f): the drain's bounded wait must observe its declared idle
    // signal ARRIVING and complete honestly — `completed: true` with a
    // waited_ms that proves the loop ACTUALLY waited. The skip-the-wait mutant
    // (treat the wait as instantly satisfied) returns in ~0ms and fails the
    // floor; the wait-for-the-bound mutant instead reports `completed: false`.
    // Review round 1 (B13) RE-ANCHORS the fixture: the idle file now lands
    // 800ms after the PAUSE SENTINEL (the file the drain's own pause command
    // writes), not 2.5s after process start — so the signal provably arrives
    // DURING this drain's wait even if the machine enters the drain late (the
    // old process-start clock could let the file predate the drain entry and
    // "complete" for the wrong reason).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        &[
            "--linger-ms",
            "600000",
            "--watch-sentinel",
            "pause.sentinel:resume.sentinel:work.log",
            // The idle signal lands 800ms after the PAUSE ANCHOR exists —
            // the anchor is written by THIS drain's own pause command, so
            // the idle file cannot predate the drain entry.
            "--idle-on-pause",
            "800:pause.sentinel:idle.sentinel",
            // The Windows graceful-stop fixture (the control_api drain
            // convention): that platform's graceful step is a pure
            // exit-watch, so a parked agent would burn the stop ladder's
            // whole default window before the forced kill — rewriting the
            // terminal cause this test pins (drain-stopped) into
            // stop-forced. The anchored self-exit ends the agent on its
            // own well past the idle signal (the wait's proof is
            // untouched) and well inside the window; a Unix drain stop is
            // a delivered signal long before the 2000ms delay, so nothing
            // changes there.
            "--exit-on-pause",
            "2000:pause.sentinel",
        ],
        Some(&["--touch", "pause.sentinel"]),
        Some(&["--touch", "resume.sentinel"]),
        Some("timeout_ms = 10000\nidle_file = \"idle.sentinel\""),
        Some("guaranteed"),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    // Let the agent get a heartbeat of work in, then drain — well before the
    // idle signal CAN land (it is anchored to the pause sentinel, which only
    // the drain's pause command writes).
    let work = sentinel(state.path(), "svc", "work.log");
    wait_for(POLL_BUDGET, "the work log to start", || {
        work_lines(&work) >= 1
    });
    assert!(
        !sentinel(state.path(), "svc", "pause.sentinel").exists(),
        "the pause anchor must not exist before the drain (the drain's own pause command writes it)"
    );
    assert!(
        !sentinel(state.path(), "svc", "idle.sentinel").exists(),
        "the idle signal must not have landed before the drain"
    );

    let report = facade.drain("svc", None).unwrap();
    assert_eq!(report.instance.state, LifecycleState::Stopped);
    assert!(
        report.outcome.completed,
        "the drain must complete on the idle signal's arrival: {:?}",
        report.outcome
    );
    assert!(
        report.outcome.waited_ms >= 500,
        "the wait must have genuinely waited for the idle signal (waited {}ms — \
         the skip-the-wait mutant reports ~0; the floor stays low so a slow \
         machine that enters the drain late still passes honestly)",
        report.outcome.waited_ms
    );
    assert!(
        report.outcome.waited_ms < 9000,
        "the drain completed well inside its 10s bound (waited {}ms)",
        report.outcome.waited_ms
    );

    // The terminal edge carries the drain-stopped cause with the TRUE record.
    let events = facade.transition_events("svc").unwrap();
    let last = events.last().expect("a stopped edge");
    assert_eq!(last.new_state, LifecycleState::Stopped);
    let cause = serde_json::to_string(&last.cause).unwrap();
    assert!(
        cause.contains("drain-stopped") && cause.contains("\"completed\":true"),
        "the terminal cause must record the completed drain: {cause}"
    );

    // The pause command of the drain ran (the cooperative pause happened on
    // the way through).
    assert!(
        sentinel(state.path(), "svc", "pause.sentinel").exists(),
        "the drain's step (a) ran the pause command"
    );
}

// ---------------------------------------------------------------------------
// (g) A drain that outlives its bound STILL stops — honestly
// ---------------------------------------------------------------------------

#[test]
fn drain_that_outlives_its_bound_still_stops() {
    // KILLER (g): the idle signal NEVER beats (no agent ever touches it), so
    // the drain must burn its declared manifest bound (1200ms) and then —
    // UNCONDITIONALLY — stop the instance anyway, reporting `completed: false`
    // with a waited_ms that brackets the bound. The stop-is-conditional mutant
    // (return early on timeout) leaves the instance alive; the fake-completion
    // mutant reports `completed: true`.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        // No `--idle-after-ms`: the declared idle file is NEVER touched.
        &[
            "--linger-ms",
            "600000",
            // The Windows graceful-stop fixture: the stop after the burnt
            // bound lands on a lingering agent, and that platform's
            // graceful step is a pure exit-watch — without a self-exit it
            // polls the whole default window and escalates to stop-forced,
            // failing this test's drain-stopped pin. The anchored exit
            // (2000ms, past the 1200ms bound so the wait genuinely times
            // out first, inside the window) ends the agent on its own; a
            // Unix drain stop is a delivered signal long before it.
            "--exit-on-pause",
            "2000:pause.sentinel",
        ],
        Some(&["--touch", "pause.sentinel"]),
        Some(&["--touch", "resume.sentinel"]),
        Some("timeout_ms = 1200\nidle_file = \"never.sentinel\""),
        Some("guaranteed"),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    let report = facade.drain("svc", None).unwrap();
    assert_eq!(
        report.instance.state,
        LifecycleState::Stopped,
        "a timed-out drain STILL stops (the last step is unconditional)"
    );
    assert!(
        !report.outcome.completed,
        "the outcome records the honest false: {:?}",
        report.outcome
    );
    assert!(
        report.outcome.waited_ms >= 1100,
        "the wait burned (at least) the declared bound: waited {}ms",
        report.outcome.waited_ms
    );
    assert!(
        report.outcome.waited_ms <= 3200,
        "the wait is BOUNDED by the declared bound (+slop): waited {}ms",
        report.outcome.waited_ms
    );

    // The terminal edge carries drain-stopped with `completed: false`.
    let events = facade.transition_events("svc").unwrap();
    let last = events.last().expect("a stopped edge");
    assert_eq!(last.new_state, LifecycleState::Stopped);
    let cause = serde_json::to_string(&last.cause).unwrap();
    assert!(
        cause.contains("drain-stopped") && cause.contains("\"completed\":false"),
        "the terminal cause must record the timed-out drain: {cause}"
    );
}

// ---------------------------------------------------------------------------
// (g2) An overrunning pause command INSIDE a drain: no pause edge, honest
// timed-out stop (review round 1, A2)
// ---------------------------------------------------------------------------

#[test]
fn drain_overrunning_pause_command_commits_no_pause_edge_and_stops_honestly() {
    // KILLER (g2, A2): when the drain's own pause command outlives the DRAIN's
    // bound, it is killed — likely BEFORE its side effect — so it is NOT a
    // pause: NO `pause-cooperative` edge may be committed (a lying edge would
    // flip every later classify_pause_kind reader: the release pass, the stop
    // ladder's cooperative skip, A1's resume gate). The row stays `running`
    // through the kill, and the composite proceeds as the TIMED-OUT path —
    // the drain still stops, recording `completed: false`. The old behavior
    // (commit the pause anyway, or reject the drain as a command failure)
    // both fail here: the pause edge would exist, or the drain would error
    // instead of stopping.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        &[
            "--linger-ms",
            "600000",
            // The Windows graceful-stop fixture, start-anchored form: this
            // fixture has NO pause anchor to hang an --exit-on-pause on —
            // the overrunning pause command is killed BEFORE its side
            // effect, so no sentinel it would write ever exists. The
            // self-exit counts from process start instead (2500ms: past
            // the 1200ms bound + the kill/reap slop the waited_ms pin
            // allows, inside the stop window). Both landings stay honest —
            // dying mid-wait is the agent-ended early edge, dying under
            // the exit-watch is a graceful stop — and both record
            // drain-stopped{false}; a Unix drain stop is a delivered
            // signal long before the delay.
            "--exit-with",
            "0:2500",
        ],
        // Touch then hang: the touch only lands if the kill FAILED to
        // precede the side effect.
        Some(&["--touch", "late.sentinel", "--hang-before", "60000"]),
        Some(&["--touch", "resume.sentinel"]),
        // The 1200ms manifest bound governs BOTH the pause command and the
        // wait (one deadline); the declared idle file is never touched by
        // anyone.
        Some("timeout_ms = 1200\nidle_file = \"never.sentinel\""),
        Some("guaranteed"),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    let report = facade.drain("svc", None).unwrap();
    assert_eq!(
        report.instance.state,
        LifecycleState::Stopped,
        "the overran-pause drain STILL stops (the last step is unconditional)"
    );
    assert!(
        !report.outcome.completed,
        "the outcome records the honest false: {:?}",
        report.outcome
    );
    assert!(
        report.outcome.waited_ms >= 1100,
        "the killed command burned (at least) the drain bound: waited {}ms",
        report.outcome.waited_ms
    );
    assert!(
        report.outcome.waited_ms <= 3200,
        "the kill + bounded reap keeps the phase bounded (+slop): waited {}ms",
        report.outcome.waited_ms
    );

    // NO pause edge — the killed command must not read as a pause, and the
    // killed command's side effect never landed.
    let events = facade.transition_events("svc").unwrap();
    assert!(
        !events.iter().any(|e| e.new_state == LifecycleState::Paused),
        "an overran pause command must never commit a running→paused edge: {events:?}"
    );
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !sentinel(state.path(), "svc", "late.sentinel").exists(),
        "the killed command must not have performed its side effect"
    );

    // The running→stopping edge names the drain; the terminal edge carries
    // the honest drain-stopped{false} record.
    let stopping = events
        .iter()
        .find(|e| e.new_state == LifecycleState::Stopping)
        .expect("a running→stopping edge");
    let stopping_cause = serde_json::to_string(&stopping.cause).unwrap();
    assert!(
        stopping_cause.contains("drain"),
        "the stopping edge names the drain command: {stopping_cause}"
    );
    let last = events.last().expect("a stopped edge");
    let cause = serde_json::to_string(&last.cause).unwrap();
    assert!(
        cause.contains("drain-stopped") && cause.contains("\"completed\":false"),
        "the terminal cause records the honest timed-out outcome: {cause}"
    );
}

// ---------------------------------------------------------------------------
// (g3) The agent ENDS mid-wait: no fabricated completion, honest early edge
// (review round 1, A3)
// ---------------------------------------------------------------------------

#[test]
fn drain_observes_the_agent_ending_mid_wait_and_never_fabricates_completion() {
    // KILLER (g3, A3): while the drain waits on its declared idle signal, the
    // AGENT may end on its own (a declared exit here). The blind file-only
    // poll mutant burns the WHOLE bound over a corpse and then records the
    // stop; the fake-completion mutant reports `completed: true` for an
    // agent that is gone. The real behavior polls the child each iteration:
    // the exit is detected promptly (well inside the bound), the "ended on
    // its own" diagnostic surfaces, and the outcome is honestly
    // `completed: false` — the terminal edge is the honest stop, never a
    // fabricated completion.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        // The agent declares its own exit 1500ms after ITS start: the drain
        // (entered immediately after start) is guaranteed inside its wait
        // loop — holding the supervision lock, so the reaper cannot process
        // the exit first — well before the exit lands.
        &["--exit-with", "0:1500", "--linger-ms", "600000"],
        Some(&["--touch", "pause.sentinel"]),
        Some(&["--touch", "resume.sentinel"]),
        // A 10s bound and an idle file nobody touches: the ONLY way out is
        // the agent-ended poll.
        Some("timeout_ms = 10000\nidle_file = \"idle.sentinel\""),
        Some("guaranteed"),
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let engine = open_with_sink(&state, &shared);
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    let report = facade.drain("svc", None).unwrap();
    assert_eq!(
        report.instance.state,
        LifecycleState::Stopped,
        "the drain still stops the (already ended) instance"
    );
    assert!(
        !report.outcome.completed,
        "an agent that ended mid-wait is NEVER a completed drain: {:?}",
        report.outcome
    );
    assert!(
        report.outcome.waited_ms >= 500,
        "the wait genuinely ran (the pause command + the polls): waited {}ms",
        report.outcome.waited_ms
    );
    assert!(
        report.outcome.waited_ms < 4000,
        "the agent-ended poll ends the wait WELL inside its 10s bound — the \
         blind file-only mutant burns the whole bound (waited {}ms)",
        report.outcome.waited_ms
    );

    // The diagnostic named the discovery (surfaced, not silent).
    let captured = sink_text(&shared);
    assert!(
        captured.contains("ended on its own while the drain waited"),
        "the agent-ended discovery must surface (sink: {captured})"
    );

    // The terminal edge is the honest drain-stopped{false} (the stop of an
    // already-exited child is NOT forced — no escalation lie either).
    let events = facade.transition_events("svc").unwrap();
    let last = events.last().expect("a stopped edge");
    assert_eq!(last.new_state, LifecycleState::Stopped);
    let cause = serde_json::to_string(&last.cause).unwrap();
    assert!(
        cause.contains("drain-stopped") && cause.contains("\"completed\":false"),
        "the terminal cause records the honest agent-ended outcome: {cause}"
    );
    assert!(
        !cause.contains("stop-forced"),
        "stopping an already-exited child must not read as a forced kill: {cause}"
    );
}

// ---------------------------------------------------------------------------
// (g4) The REAL escalation drill: a TERM-immune agent forces the stop ladder
// through the graceful window, and the drain outcome folds into the forced
// cause (review round 1, B7)
// ---------------------------------------------------------------------------

#[test]
fn drain_over_a_term_immune_agent_escalates_and_folds_the_outcome_into_the_forced_cause() {
    // KILLER (g4, B7): the stop ladder's escalation path with a drain riding
    // it. The agent ignores SIGTERM (`trap '' TERM`), so the graceful window
    // (the engine default 30s — the drain passes no override) MUST elapse and
    // escalate to the forced kill; the terminal cause is `stop-forced` (forced
    // honesty outranks the drain bookkeeping) with the drain's timed-out
    // outcome FOLDED INTO the detail — the composite's result stays durable
    // through the escalation. The mutant that swaps the cause to
    // drain-stopped on a forced stop fails the cause assert; the mutant that
    // drops the folded outcome fails the detail assert. Unix-only data-driven
    // skip (the drill needs POSIX traps; the escalation path itself is
    // cross-OS and covered structurally elsewhere).
    if OsId::current() == OsId::Windows {
        return;
    }
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // Hand-rolled manifest: the subject is `sh` with a trapped TERM, not
    // fake_agent (which dies on TERM like any unhandled default).
    let body = r#"
contract_version = "1.1.0"

[adapter]
kind = "svc"

[lifecycle.start]
exec = "sh"
args = ["-c", "trap '' TERM; sleep 60"]

[lifecycle.pause]
exec = "sh"
args = ["-c", "touch pause.sentinel"]

[lifecycle.resume]
exec = "sh"
args = ["-c", "touch resume.sentinel"]

[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"

[metering]
source = "self-reported"

[drain]
timeout_ms = 500
idle_file = "never.sentinel"
"#;
    std::fs::write(manifest.path().join("adapter.toml"), body).unwrap();

    let engine = open(&state);
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    let report = facade.drain("svc", None).unwrap();
    assert_eq!(
        report.instance.state,
        LifecycleState::Stopped,
        "the escalated drain still lands stopped"
    );
    assert!(
        !report.outcome.completed,
        "the bounded wait timed out (nobody touches the idle file): {:?}",
        report.outcome
    );
    assert!(
        report.outcome.waited_ms < 2000,
        "waited_ms measures the BOUNDED PHASE (pause command + wait), not the \
         stop ladder's 30s window (waited {}ms)",
        report.outcome.waited_ms
    );
    // The drain's own pause command ran (the drill's pause is a real `sh`).
    assert!(
        sentinel(state.path(), "svc", "pause.sentinel").exists(),
        "the drain's pause command ran before the wait"
    );

    // The terminal cause is stop-forced WITH the folded drain outcome.
    let events = facade.transition_events("svc").unwrap();
    let last = events.last().expect("a stopped edge");
    assert_eq!(last.new_state, LifecycleState::Stopped);
    let cause = serde_json::to_string(&last.cause).unwrap();
    assert!(
        cause.contains("stop-forced"),
        "a TERM-immune agent escalates — the forced cause outranks the drain \
         bookkeeping: {cause}"
    );
    assert!(
        cause.contains("the drain's bounded wait timed out after")
            && cause.contains("recorded here because the stop escalated"),
        "the forced cause folds the drain's timed-out outcome into its detail: {cause}"
    );
}

// ---------------------------------------------------------------------------
// (g5) A STALE idle file is NOT an idle signal (freshness, not existence)
// (review round 1, B11)
// ---------------------------------------------------------------------------

#[test]
fn drain_reads_a_stale_idle_file_as_not_idle_and_burns_the_bound() {
    // KILLER (g5, B11): the idle signal is FRESHNESS, not existence — a file
    // that predates the drain's wait by more than the engine's freshness
    // slack (2s) reads NOT idle, and the drain must burn its bound and record
    // the honest `completed: false`. The existence-reading mutant completes
    // immediately (the file is right there), faking a drained agent that
    // never drained.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        &[
            "--linger-ms",
            "600000",
            // The Windows graceful-stop fixture: the stop after the burnt
            // bound lands on a lingering agent, and that platform's
            // graceful step is a pure exit-watch — without a self-exit it
            // polls the whole default window and escalates to stop-forced,
            // failing this test's drain-stopped pin. 2000ms is past the
            // 1200ms bound (the wait genuinely burns first) and inside the
            // window; the 3000ms freshness sleep above happens BEFORE the
            // drain, so it never delays the anchor the delay counts from.
            "--exit-on-pause",
            "2000:pause.sentinel",
        ],
        Some(&["--touch", "pause.sentinel"]),
        Some(&["--touch", "resume.sentinel"]),
        Some("timeout_ms = 1200\nidle_file = \"idle.sentinel\""),
        Some("guaranteed"),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    // The idle file exists — but it was written NOW, and the drain enters
    // more than the freshness slack (2s) later, so it reads STALE.
    let idle = sentinel(state.path(), "svc", "idle.sentinel");
    std::fs::write(&idle, b"stale").unwrap();
    std::thread::sleep(Duration::from_millis(3000));

    let report = facade.drain("svc", None).unwrap();
    assert_eq!(report.instance.state, LifecycleState::Stopped);
    assert!(
        !report.outcome.completed,
        "a stale idle file is NOT an idle signal: {:?}",
        report.outcome
    );
    assert!(
        report.outcome.waited_ms >= 1100,
        "the drain burned (at least) its declared bound over the stale file: \
         waited {}ms",
        report.outcome.waited_ms
    );
    assert!(
        report.outcome.waited_ms <= 3200,
        "the wait is BOUNDED by the declared bound (+slop): waited {}ms",
        report.outcome.waited_ms
    );

    // The terminal edge carries the honest false.
    let events = facade.transition_events("svc").unwrap();
    let last = events.last().expect("a stopped edge");
    let cause = serde_json::to_string(&last.cause).unwrap();
    assert!(
        cause.contains("drain-stopped") && cause.contains("\"completed\":false"),
        "the terminal cause records the stale-idle timeout: {cause}"
    );
}

// ---------------------------------------------------------------------------
// (h) The degenerate drain — no command, no signal: stop directly, and SAY so
// ---------------------------------------------------------------------------

#[test]
fn degenerate_drain_stops_directly_and_says_so() {
    // KILLER (h): a manifest with NEITHER lifecycle commands NOR a `[drain]`
    // section has NO declared wait at all — the drain must stop DIRECTLY with
    // `completed: true, waited_ms: 0` and emit the honest "no declared wait"
    // diagnostic (surfaced, not silent). The no-degenerate-check mutant runs a
    // phantom 30s wait (or records a nonzero waited_ms with no diagnostic).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        &["--linger-ms", "600000"],
        None,
        None,
        None,
        Some("guaranteed"),
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let engine = open_with_sink(&state, &shared);
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    let report = facade.drain("svc", None).unwrap();
    assert_eq!(report.instance.state, LifecycleState::Stopped);
    assert!(
        report.outcome.completed && report.outcome.waited_ms == 0,
        "the degenerate drain completes with ZERO wait: {:?}",
        report.outcome
    );
    let captured = sink_text(&shared);
    assert!(
        captured.contains("no declared wait"),
        "the degenerate drain must SAY it had nothing to wait for (sink: {captured})"
    );
}

// ---------------------------------------------------------------------------
// (i) Pair-without-[drain]: the default bound is a CEILING, and the stop
// ladder skips the OS resume signal for a cooperative pause
// ---------------------------------------------------------------------------

#[test]
fn pair_without_drain_section_drains_under_the_default_bound() {
    // KILLER (i): a manifest with the pause/resume COMMAND pair but NO
    // `[drain]` section drains under the ENGINE DEFAULT bound — which must be
    // a CEILING (the drain completes in bounded time, never a floor it waits
    // out), and the stop ladder must SKIP the OS resume signal (a cooperative
    // pause has no OS suspension to undo) while SAYING so — the honest skip
    // note in the sink. The bound-as-floor mutant waits the full 30s; the
    // signal-sending mutant omits the note and resumes the process.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        &[
            "--linger-ms",
            "600000",
            "--watch-sentinel",
            "pause.sentinel:resume.sentinel:work.log",
        ],
        Some(&["--touch", "pause.sentinel"]),
        Some(&["--touch", "resume.sentinel"]),
        None,
        Some("guaranteed"),
    );

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let engine = open_with_sink(&state, &shared);
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    let report = facade.drain("svc", None).unwrap();
    assert_eq!(report.instance.state, LifecycleState::Stopped);
    assert!(
        report.outcome.completed,
        "a command-only drain (no idle signal) completes: {:?}",
        report.outcome
    );
    assert!(
        report.outcome.waited_ms < 5000,
        "the default bound is a CEILING — the drain must not wait it out (waited {}ms)",
        report.outcome.waited_ms
    );

    // The ladder's honest cooperative-skip note: the drain's own pause made
    // the instance cooperatively paused, so the stop skipped the OS resume
    // signal — and said so. Review round 1 (B10): and it must NOT fire the
    // signal-path note — no resume was attempted, so claiming one would be
    // the lie the skip exists to prevent.
    let captured = sink_text(&shared);
    assert!(
        captured.contains("paused COOPERATIVELY"),
        "the stop ladder must say it skipped the OS resume signal (sink: {captured})"
    );
    assert!(
        !captured.contains("resumed the paused process before the stop ladder"),
        "a cooperative pause's stop must not claim a signal resume (sink: {captured})"
    );
}

// ---------------------------------------------------------------------------
// (j) The liveness skip holds for a cooperative pause
// ---------------------------------------------------------------------------

#[test]
fn liveness_skip_holds_for_a_cooperative_pause() {
    // KILLER (j): a cooperatively-paused instance's heartbeat CAN go stale
    // (its process keeps running, but its declared signal may stop) — the
    // reaper's liveness evaluation must still SKIP the row (only an EXACTLY
    // `running` instance is probed; a pause is a normal state, not a
    // degradation). The probe-paused-rows mutant fails the row (and the
    // restart policy fires) while the instance sits paused.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        // Beat every 50ms until 900ms from start, then stop — the heart goes
        // stale WHILE the instance is cooperatively paused (the pause lands
        // ~400ms, the heart stops at 900ms, the 600ms window expires ~1500ms).
        &[
            "--heartbeat-file",
            "hb.marker:50",
            "--heartbeat-stop-after",
            "900",
            "--linger-ms",
            "600000",
        ],
        Some(&["--touch", "pause.sentinel"]),
        Some(&["--touch", "resume.sentinel"]),
        None,
        Some("guaranteed"),
    );
    // Add the [liveness.heartbeat_file] section (the sub-table header implies
    // the empty parent `[liveness]` table — the liveness.rs convention). The
    // lifecycle writer does not carry that section, so splice the raw body in
    // before the final [metering] (TOML section order is irrelevant; this
    // keeps the file human-readable).
    let path = manifest.path().join("adapter.toml");
    let body = std::fs::read_to_string(&path).unwrap();
    let liveness = "\n[liveness.heartbeat_file]\npath = \"hb.marker\"\nmax_age_ms = 600\n";
    let body = body.replace("\n[metering]", &format!("{liveness}\n[metering]"));
    std::fs::write(&path, body).unwrap();

    let engine = open(&state);
    let facade = engine.blocking();
    register_and_start(&facade, &manifest, "svc");

    // The heart has beaten (the first beat is immediate).
    let hb = sentinel(state.path(), "svc", "hb.marker");
    wait_for(POLL_BUDGET, "the heartbeat file to appear", || hb.exists());

    // Cooperatively pause (~well before the heart stops at 900ms from agent
    // start — the engine's readiness-bounded start keeps this inside the
    // window on any machine start() can succeed on).
    let paused = facade.pause("svc").unwrap();
    assert_eq!(paused.state, LifecycleState::Paused);

    // Well past staleness (900ms) + window (600ms) + reaper cadence: the row
    // must STILL be paused — probed, it would have failed (and restarted) by
    // now.
    std::thread::sleep(Duration::from_millis(1400));
    let status = facade.instance_status("svc").unwrap();
    assert_eq!(
        status.instance.state,
        LifecycleState::Paused,
        "a stale heartbeat must not fail a cooperatively-paused row"
    );
    assert_eq!(
        status.restart_count, 0,
        "no restart may fire for a paused row"
    );

    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
}

// ---------------------------------------------------------------------------
// The gates: drain REFUSES non-running instances and an out-of-range bound
// ---------------------------------------------------------------------------

#[test]
fn drain_refuses_non_running_instances_and_out_of_range_bounds() {
    // The RUNNING-ONLY gate: a REGISTERED (never started) instance and a
    // PAUSED instance both refuse the drain with `DrainRefused` naming the
    // state — the transition gate precedes every side effect. And the bound
    // override's range (1..=600_000ms) is engine-enforced too: zero and
    // past-the-ceiling bounds refuse with `DrainBoundInvalid` naming the bound.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_drain_manifest(
        manifest.path(),
        "svc",
        &["--linger-ms", "600000"],
        Some(&["--touch", "pause.sentinel"]),
        Some(&["--touch", "resume.sentinel"]),
        Some("timeout_ms = 5000"),
        Some("guaranteed"),
    );

    let engine = open(&state);
    let facade = engine.blocking();

    // Registered, never started.
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let result = facade.drain("svc", None);
    let Err(EngineError::DrainRefused {
        name,
        state: refused_state,
    }) = &result
    else {
        panic!("drain on a registered instance must refuse: {result:?}");
    };
    assert_eq!(name, "svc");
    assert_eq!(refused_state, "registered");

    // Running: the bound override's range is enforced (before any side
    // effect — the bound test needs a running instance to get past gate 1).
    let started = facade.start("svc").unwrap();
    assert_eq!(started.state, LifecycleState::Running);
    for bound_ms in [0u64, 600_001] {
        let result = facade.drain("svc", Some(Duration::from_millis(bound_ms)));
        let Err(EngineError::DrainBoundInvalid { bound_ms: reported }) = &result else {
            panic!("drain with bound {bound_ms}ms must refuse: {result:?}");
        };
        assert_eq!(*reported, bound_ms, "the refusal names the bound");
    }
    // The instance is untouched by the refused drains.
    assert_eq!(
        facade.instance_status("svc").unwrap().instance.state,
        LifecycleState::Running
    );

    // Paused (cooperatively, via the declared command).
    facade.pause("svc").unwrap();
    let result = facade.drain("svc", None);
    let Err(EngineError::DrainRefused {
        state: refused_state,
        ..
    }) = &result
    else {
        panic!("drain on a paused instance must refuse: {result:?}");
    };
    assert_eq!(refused_state, "paused");

    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();

    // STOPPED (review round 1, B6): a cleanly stopped instance refuses too —
    // the drain is a running-instance composite, not a re-stop.
    let result = facade.drain("svc", None);
    let Err(EngineError::DrainRefused {
        state: refused_state,
        ..
    }) = &result
    else {
        panic!("drain on a stopped instance must refuse: {result:?}");
    };
    assert_eq!(refused_state, "stopped");

    // FAILED (review round 1, B6): a crashed instance (immediate exit under a
    // `never` restart policy) refuses the drain with the failed state named —
    // a drain must never resurrect or re-stop a dead row.
    let failed_manifest = TempDir::new().unwrap();
    write_drain_manifest(
        failed_manifest.path(),
        "mortal",
        &["--exit-fast", "3"],
        None,
        None,
        None,
        Some("guaranteed"),
    );
    facade
        .register_with_adapter(
            "mortal",
            &AdapterRef::Manifest(failed_manifest.path().to_path_buf()),
        )
        .unwrap();
    facade
        .set_restart_policy("mortal", RestartPolicy::Never)
        .expect("set restart policy");
    let _ = facade.start("mortal");
    wait_for(POLL_BUDGET, "the mortal instance to land failed", || {
        facade
            .instance_status("mortal")
            .map(|s| s.instance.state)
            .ok()
            == Some(LifecycleState::Failed)
    });
    let result = facade.drain("mortal", None);
    let Err(EngineError::DrainRefused {
        state: refused_state,
        ..
    }) = &result
    else {
        panic!("drain on a failed instance must refuse: {result:?}");
    };
    assert_eq!(refused_state, "failed");
}
