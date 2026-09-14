//! Integration tests for the story-1.5 pause/resume with honest, per-OS
//! semantics (FR-7 — the "surfaced not silent" HONESTY story), driven through
//! the engine's PUBLIC async API + blocking facade only (spine AD-2/AD-13),
//! spawning the REAL `fake_agent` helper so suspension is genuinely exercised.
//!
//! The three levels are proven:
//! * **Guaranteed (Unix)** — a real SIGSTOP suspension is PROVABLE: the
//!   `fake_agent` heartbeat stops growing while paused and resumes after SIGCONT,
//!   with states `running→paused→running`. Runtime-skipped on Windows (the
//!   guaranteed path there is not applicable) via the data-driven `OsId` — NO
//!   `#[cfg]` here (this file is outside the backends allowlist), mirroring the
//!   1-4 `stop_escalates_to_forced_kill_and_records_it` skip.
//! * **Best-effort** — the transition proceeds AND the emitted transition event
//!   carries the `pause-best-effort` qualifier cause (the machine-readable half
//!   of "surfaced not silent").
//! * **Unsupported** — pause FAILS FAST with `EngineError::CapabilityUnsupported`,
//!   the state is UNCHANGED, and NO transition event is appended.

use std::path::Path;
use std::time::{Duration, Instant};

use ktesio_engine::{AdapterRef, Engine, LifecycleState, OsId};
use tempfile::TempDir;

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

/// Write a manifest whose `[lifecycle.start]` exec points at `fake_agent` with
/// `args`, and whose `[capabilities.pause]` declares `pause_level` for the CURRENT
/// OS (so the effective projection is exactly `pause_level` at read time). Always
/// declares `interaction` guaranteed everywhere so registration passes.
fn write_pause_manifest(dir: &Path, kind: &str, args: &[&str], pause_level_current_os: &str) {
    let bin = ktesio_conformance::fake_agent_bin();
    let args_toml = args
        .iter()
        .map(|a| format!("{a:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    // Declare pause ONLY for the current OS at the requested level. For the
    // "unsupported" proof the caller passes a body with no current-OS entry.
    let body = format!(
        r#"
contract_version = "1.0.0"

[adapter]
kind = "{kind}"

[lifecycle.start]
exec = {exec:?}
args = [{args_toml}]

[capabilities.pause]
{os} = "{pause_level_current_os}"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#,
        exec = bin.to_string_lossy(),
        os = current_os_key(),
    );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

/// Open an engine over a fresh temp state dir.
fn open(base: &TempDir) -> Engine {
    Engine::open(Some(base.path().to_path_buf())).expect("open engine")
}

/// Count `heartbeat <n>` lines in the agent's captured output log.
fn heartbeat_lines(agent_log: &Path) -> usize {
    std::fs::read_to_string(agent_log)
        .map(|c| c.lines().filter(|l| l.starts_with("heartbeat ")).count())
        .unwrap_or(0)
}

#[test]
fn guaranteed_pause_really_suspends_then_resume_wakes_it_unix() {
    // AC1 + AC7(a): on a Unix host where pause projects to `guaranteed`, pausing
    // the running instance delivers a REAL SIGSTOP — the heartbeat STOPS growing
    // while paused — and resume (SIGCONT) wakes it; states go
    // running→paused→running, and the transition events record plain
    // command/pause + command/resume causes (NO best-effort qualifier — it is a
    // true suspension).
    //
    // Runtime-skip on Windows (the guaranteed signal path is Unix-only; Windows
    // pause is best-effort). NO `#[cfg]` — data-driven skip (mirrors 1-4).
    if OsId::current() == OsId::Windows {
        return;
    }
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_pause_manifest(
        manifest.path(),
        "svc",
        &["--heartbeat-ms", "50", "--linger-ms", "600000"],
        "guaranteed",
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let started = facade.start("svc").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // The agent's captured stdout is <home>/logs/agent.log.
    let agent_log = state
        .path()
        .join("agents")
        .join("svc")
        .join("logs")
        .join("agent.log");

    // Wait for the heartbeat to start ticking.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if heartbeat_lines(&agent_log) >= 2 {
            break;
        }
        assert!(Instant::now() < deadline, "heartbeat never started");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Pause → paused, and the heartbeat freezes (real SIGSTOP suspension).
    //
    // Robustness under scheduler jitter (LOW-3): SIGSTOP delivery + the child's
    // in-flight 25ms poll can let ONE last line land after pause returns, and a
    // heavily loaded runner (e.g. tarpaulin running the suite concurrently) can
    // delay the sample. So we do NOT compare two instantaneous samples. Instead
    // we settle briefly, snapshot a BASELINE, then watch across a LONG window
    // (1s ≫ many 50ms heartbeat intervals) and require the count NEVER exceeds
    // baseline. This tolerates jitter (a stuck-but-alive scheduler cannot make a
    // SUSPENDED process emit) yet stays a GENUINE suspension proof: if the pause
    // (SIGSTOP) were removed, a live 50ms heartbeat would emit ~20 lines across
    // this window and blow past baseline on the very first poll — the assert
    // below would fire. (The resume step further requires renewed growth.)
    let paused = facade.pause("svc").unwrap();
    assert_eq!(paused.state, LifecycleState::Paused);
    std::thread::sleep(Duration::from_millis(200)); // let SIGSTOP + any in-flight line settle
    let baseline = heartbeat_lines(&agent_log);
    let watch_until = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < watch_until {
        let now = heartbeat_lines(&agent_log);
        assert!(
            now <= baseline,
            "heartbeat must NOT grow while paused (real SIGSTOP): baseline {baseline}, saw {now}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let after = heartbeat_lines(&agent_log);
    assert_eq!(
        after, baseline,
        "heartbeat count must be unchanged across the whole paused window: {baseline} → {after}"
    );

    // Resume → running, and the heartbeat grows again (SIGCONT).
    let resumed = facade.resume("svc").unwrap();
    assert_eq!(resumed.state, LifecycleState::Running);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if heartbeat_lines(&agent_log) > after {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "heartbeat must resume growing after SIGCONT (stuck at {after})"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Teardown: stop the instance.
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();

    // AC1 event sequence: registered→starting→running→paused→running→stopping→stopped,
    // with the pause/resume transitions carrying plain command causes.
    let events = facade.transition_events("svc").unwrap();
    let states: Vec<(LifecycleState, LifecycleState)> = events
        .iter()
        .map(|e| (e.prior_state, e.new_state))
        .collect();
    assert_eq!(
        states,
        vec![
            (LifecycleState::Registered, LifecycleState::Starting),
            (LifecycleState::Starting, LifecycleState::Running),
            (LifecycleState::Running, LifecycleState::Paused),
            (LifecycleState::Paused, LifecycleState::Running),
            (LifecycleState::Running, LifecycleState::Stopping),
            (LifecycleState::Stopping, LifecycleState::Stopped),
        ],
        "events: {events:#?}"
    );
    // The pause + resume causes are plain commands (no best-effort qualifier).
    let pause_evt = events
        .iter()
        .find(|e| e.new_state == LifecycleState::Paused)
        .unwrap();
    let pause_cause = serde_json::to_string(&pause_evt.cause).unwrap();
    assert!(
        pause_cause.contains("\"kind\":\"command\"") && pause_cause.contains("pause"),
        "guaranteed pause must be a plain command cause, got {pause_cause}"
    );
    assert!(
        !pause_cause.contains("best-effort"),
        "guaranteed pause must carry NO best-effort qualifier: {pause_cause}"
    );
    // Symmetric check (NIT-2): the resume transition's cause is likewise the
    // plain `command`/`resume` — a guaranteed resume is a true SIGCONT, so it
    // carries NO best-effort qualifier either.
    let resume_evt = events
        .iter()
        .find(|e| e.prior_state == LifecycleState::Paused && e.new_state == LifecycleState::Running)
        .unwrap();
    let resume_cause = serde_json::to_string(&resume_evt.cause).unwrap();
    assert!(
        resume_cause.contains("\"kind\":\"command\"") && resume_cause.contains("resume"),
        "guaranteed resume must be a plain command cause, got {resume_cause}"
    );
    assert!(
        !resume_cause.contains("best-effort"),
        "guaranteed resume must carry NO best-effort qualifier: {resume_cause}"
    );
}

#[test]
fn best_effort_pause_transitions_and_surfaces_the_qualifier_in_the_event() {
    // AC2 + AC7(b): on any host, a manifest whose CURRENT-OS pause level is
    // `best-effort` transitions running→paused (the pause proceeds cooperatively)
    // AND the emitted transition event carries the `pause-best-effort` qualifier
    // cause — never a silent success. The CLI half (stderr note) is proven in the
    // kt CLI test (Task 12).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_pause_manifest(
        manifest.path(),
        "be",
        &["--linger-ms", "600000"],
        "best-effort",
    );

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("be", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    facade.start("be").unwrap();

    let paused = facade.pause("be").unwrap();
    assert_eq!(
        paused.state,
        LifecycleState::Paused,
        "best-effort pause still transitions to paused"
    );

    // The transition event carries the pause-best-effort qualifier (AC2 machine
    // half).
    let events = facade.transition_events("be").unwrap();
    let pause_evt = events
        .iter()
        .find(|e| e.new_state == LifecycleState::Paused)
        .expect("a running→paused event exists");
    let cause = serde_json::to_string(&pause_evt.cause).unwrap();
    assert!(
        cause.contains("\"kind\":\"pause-best-effort\""),
        "best-effort pause must carry the pause-best-effort cause tag, got {cause}"
    );

    // And resume carries the resume-best-effort qualifier symmetrically.
    let resumed = facade.resume("be").unwrap();
    assert_eq!(resumed.state, LifecycleState::Running);
    let events = facade.transition_events("be").unwrap();
    let resume_evt = events
        .iter()
        .rev()
        .find(|e| e.new_state == LifecycleState::Running && e.prior_state == LifecycleState::Paused)
        .expect("a paused→running event exists");
    let cause = serde_json::to_string(&resume_evt.cause).unwrap();
    assert!(
        cause.contains("\"kind\":\"resume-best-effort\""),
        "best-effort resume must carry the resume-best-effort cause tag, got {cause}"
    );

    // Teardown.
    facade.stop("be", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn unsupported_pause_fails_fast_with_no_state_change_and_no_event() {
    // AC3 + AC7(c): a manifest whose CURRENT-OS pause level projects to
    // `unsupported` (here: pause declared ONLY for an OS that is NOT the running
    // one, so the current-OS projection is the honest Unsupported default) makes
    // `pause` FAIL FAST with EngineError::CapabilityUnsupported — the instance
    // state is UNCHANGED (still running) and NO transition event was appended for
    // the failed pause.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // Declare pause for an OS OTHER than the current one so the current-OS
    // projection is Unsupported. Pick a modeled OS that is not us.
    let other_os = match OsId::current() {
        OsId::Linux => "windows",
        OsId::Macos => "windows",
        OsId::Windows => "linux",
        OsId::Other => "linux",
    };
    let bin = ktesio_conformance::fake_agent_bin();
    let body = format!(
        r#"
contract_version = "1.0.0"

[adapter]
kind = "unsup"

[lifecycle.start]
exec = {exec:?}
args = ["--linger-ms", "600000"]

[capabilities.pause]
{other} = "guaranteed"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#,
        exec = bin.to_string_lossy(),
        other = other_os,
    );
    std::fs::write(manifest.path().join("adapter.toml"), body).unwrap();

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "unsup",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade.start("unsup").unwrap();

    // Capture the event count BEFORE the failed pause, to prove none is appended.
    let before = facade.transition_events("unsup").unwrap();
    let before_len = before.len();

    let err = facade.pause("unsup").unwrap_err();
    let msg = err.to_string();
    // The diagnostic quotes the declaration: names pause, the current OS, and
    // the `unsupported` level (AC3).
    assert!(msg.contains("cannot pause"), "{msg}");
    assert!(msg.contains("unsupported"), "{msg}");
    assert!(msg.contains(current_os_key()), "{msg}");

    // State UNCHANGED (still running).
    let listed = facade.list().unwrap();
    let inst = listed.iter().find(|i| i.name.as_str() == "unsup").unwrap();
    assert_eq!(
        inst.state,
        LifecycleState::Running,
        "unsupported pause must not change state"
    );

    // NO transition event appended for the failed pause.
    let after = facade.transition_events("unsup").unwrap();
    assert_eq!(
        after.len(),
        before_len,
        "no event may be appended for a failed (unsupported) pause"
    );

    // Teardown.
    facade.stop("unsup", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn pause_on_a_registered_instance_is_the_uniform_invalid_transition() {
    // AC4: pause on a NOT-running instance (registered, never started) rejects
    // with the uniform InvalidTransition — BEFORE any level read or side effect,
    // identical across adapters (it comes from the shared transition table).
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let facade = engine.blocking();
    facade.register("nat", "mock").unwrap();
    let err = facade.pause("nat").unwrap_err();
    assert!(err.to_string().contains("cannot pause"), "{err}");
    // No events recorded (the rejection is before any transition).
    assert!(facade.transition_events("nat").unwrap().is_empty());
}

#[test]
fn resume_on_a_running_instance_is_the_uniform_invalid_transition() {
    // AC4: resume on a running (not paused) instance rejects with the uniform
    // InvalidTransition.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_pause_manifest(
        manifest.path(),
        "svc",
        &["--linger-ms", "600000"],
        "best-effort",
    );
    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    facade.start("svc").unwrap();
    let err = facade.resume("svc").unwrap_err();
    assert!(err.to_string().contains("cannot resume"), "{err}");
    // Teardown.
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn stop_from_paused_reaches_stopped() {
    // AC4 (spine diagram paused --> stopping): a paused instance is stoppable.
    // Use best-effort so it works on any host (no real suspension needed).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_pause_manifest(
        manifest.path(),
        "svc",
        &["--linger-ms", "600000"],
        "best-effort",
    );
    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    facade.start("svc").unwrap();
    facade.pause("svc").unwrap();
    // Stop directly from paused.
    let stopped = facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
    assert_eq!(stopped.state, LifecycleState::Stopped);
    let events = facade.transition_events("svc").unwrap();
    // The transition just before stopped is paused→stopping.
    assert!(
        events
            .iter()
            .any(|e| e.prior_state == LifecycleState::Paused
                && e.new_state == LifecycleState::Stopping),
        "a paused→stopping transition must be recorded: {events:#?}"
    );
}

#[test]
fn guaranteed_pause_without_an_in_memory_handle_is_a_no_op_transition() {
    // Cross-lifetime honesty (single-lifetime boundary, AD-5 is story 1-6): an
    // instance whose row says `running` but for which THIS engine holds no
    // process handle (e.g. it was started by a prior engine) still transitions to
    // `paused` on a GUARANTEED pause — we cannot SIGSTOP a process we do not
    // hold, so the signal is a documented best-effort no-op while the state
    // change proceeds. Mirrors the 1-4 stop-without-a-handle no-op test.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_pause_manifest(
        manifest.path(),
        "svc",
        &["--linger-ms", "600000"],
        "guaranteed",
    );
    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    // Force the row to `running` directly in the state DB (no in-memory handle).
    {
        let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
        let n = conn
            .execute(
                "UPDATE agent_instances SET state = 'running' WHERE name = 'svc'",
                [],
            )
            .unwrap();
        assert_eq!(n, 1);
    }
    // Pause: no handle → guaranteed transition to paused (best-effort no-op signal).
    let paused = facade.pause("svc").unwrap();
    assert_eq!(paused.state, LifecycleState::Paused);
    // AI-8: the recorded cause is the HONEST best-effort qualifier naming the
    // missing handle — never a plain `pause` command that would read as a real
    // suspension the engine could not perform.
    let events = facade.transition_events("svc").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.new_state, LifecycleState::Paused);
    let cause = serde_json::to_string(&last.cause).unwrap();
    assert!(
        cause.contains("\"kind\":\"pause-best-effort\""),
        "a guaranteed pause with no in-memory handle must record the best-effort \
         qualifier, not a plain command: {cause}"
    );
    assert!(
        cause.contains("no live process handle"),
        "the qualifier must name the missing handle (the honest why): {cause}"
    );
}

#[test]
fn resume_under_an_unsupported_pause_declaration_names_the_state_and_remediation() {
    // AI-7: `resume` on an instance that is `paused` while its adapter declares
    // PAUSE `unsupported` on this OS fails fast with the DEDICATED
    // ResumeUnsupported diagnostic — naming the STATE (`paused`) + the
    // declaration (level + OS) and pointing at `stop` + `start` (the real escape
    // hatch; stop never consults the pause level) — instead of the bare
    // pause-unsupported error that would strand the operator. NO state change,
    // NO event appended.
    //
    // The paused row is reached the honest way this scenario occurs: the row was
    // paused under a PRIOR declaration/OS (here: forced directly in the state
    // DB, the established pattern of
    // `guaranteed_pause_without_an_in_memory_handle...`), while the CURRENT
    // declaration projects `unsupported`.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // Declare pause for an OS OTHER than the current one so the current-OS
    // projection is Unsupported.
    let other_os = match OsId::current() {
        OsId::Linux => "windows",
        OsId::Macos => "windows",
        OsId::Windows => "linux",
        OsId::Other => "linux",
    };
    let bin = ktesio_conformance::fake_agent_bin();
    let body = format!(
        r#"
contract_version = "1.0.0"

[adapter]
kind = "unsupres"

[lifecycle.start]
exec = {exec:?}
args = ["--linger-ms", "600000"]

[capabilities.pause]
{other} = "guaranteed"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#,
        exec = bin.to_string_lossy(),
        other = other_os,
    );
    std::fs::write(manifest.path().join("adapter.toml"), body).unwrap();

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "unsupres",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    // Force the row to `paused` directly in the state DB (the prior-declaration
    // drift scenario).
    {
        let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
        let n = conn
            .execute(
                "UPDATE agent_instances SET state = 'paused' WHERE name = 'unsupres'",
                [],
            )
            .unwrap();
        assert_eq!(n, 1);
    }
    let before = facade.transition_events("unsupres").unwrap().len();

    let err = facade.resume("unsupres").unwrap_err();
    let msg = err.to_string();
    // Names the STATE + the declaration (level + OS) — never a bare
    // "cannot pause" that would strand a paused instance.
    assert!(msg.contains("paused"), "names the state: {msg}");
    assert!(msg.contains("unsupported"), "quotes the level: {msg}");
    assert!(msg.contains(current_os_key()), "names the OS: {msg}");
    // The remediation names the real escape hatch (stop + start).
    assert!(
        msg.contains("stop") && msg.contains("start"),
        "names the stop/start remediation: {msg}"
    );

    // State UNCHANGED (still paused — fail fast, no fake success).
    let listed = facade.list().unwrap();
    let inst = listed
        .iter()
        .find(|i| i.name.as_str() == "unsupres")
        .unwrap();
    assert_eq!(
        inst.state,
        LifecycleState::Paused,
        "the unsupported-declaration resume must not change state"
    );
    // NO transition event appended for the failed resume.
    let after = facade.transition_events("unsupres").unwrap();
    assert_eq!(
        after.len(),
        before,
        "no event may be appended for a failed (resume-unsupported) resume"
    );
}

#[test]
fn guaranteed_resume_without_an_in_memory_handle_records_the_qualifier() {
    // AI-8 (the loop-1 sibling of the guaranteed-pause-without-a-handle proof):
    // a GUARANTEED resume of a `paused` row for which THIS engine holds no
    // process handle still transitions — and the recorded cause must be the
    // honest `resume-best-effort` qualifier naming the missing handle, never a
    // plain `resume` command that would read as a performed wake-up. The row is
    // forced to `paused` directly in the state DB (the established pattern of
    // `guaranteed_pause_without_an_in_memory_handle...`).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_pause_manifest(
        manifest.path(),
        "svc",
        &["--linger-ms", "600000"],
        "guaranteed",
    );
    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    {
        let conn = rusqlite::Connection::open(state.path().join("state.db")).unwrap();
        let n = conn
            .execute(
                "UPDATE agent_instances SET state = 'paused' WHERE name = 'svc'",
                [],
            )
            .unwrap();
        assert_eq!(n, 1);
    }
    let resumed = facade.resume("svc").unwrap();
    assert_eq!(resumed.state, LifecycleState::Running);
    let events = facade.transition_events("svc").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.new_state, LifecycleState::Running);
    let cause = serde_json::to_string(&last.cause).unwrap();
    assert!(
        cause.contains("\"kind\":\"resume-best-effort\""),
        "a guaranteed resume with no in-memory handle must record the best-effort \
         qualifier, not a plain command: {cause}"
    );
    assert!(
        cause.contains("no live process handle"),
        "the qualifier must name the missing handle (the honest why): {cause}"
    );
}

#[test]
fn pause_on_missing_instance_is_not_found() {
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let err = engine.blocking().pause("ghost").unwrap_err();
    assert!(err.to_string().contains("ghost"), "{err}");
}

#[test]
fn resume_on_invalid_name_is_rejected() {
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let err = engine.blocking().resume("Bad Name").unwrap_err();
    assert!(err.to_string().contains("invalid"), "{err}");
}
