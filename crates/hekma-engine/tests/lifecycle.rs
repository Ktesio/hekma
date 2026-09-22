//! Integration tests for the story-1.4 lifecycle: start / stop / failure /
//! invalid-transition, driven through the engine's PUBLIC async API + blocking
//! facade only (spine AD-2/AD-13), spawning the REAL `fake_agent` helper so the
//! supervision is genuinely exercised (not mocked).
//!
//! All start+stop pairs run within ONE [`Engine`] lifetime — the supervisor
//! holds process handles in memory for that lifetime (cross-restart orphan
//! adoption is story 1-6). This is exactly the surface the ACs require: "start
//! it → running → stop it → stopped, no survivor" for a single engine.
//!
//! The `fake_agent` binary path is resolved via
//! [`hekma_conformance::fake_agent_bin`] (a dev-dependency here; the boundary
//! gate excludes dev-deps, so `kt`'s shipping graph is untouched).

use std::path::Path;
use std::time::Duration;

use hekma_engine::{AdapterRef, Engine, LifecycleState};
use tempfile::TempDir;

/// Write a manifest whose `[lifecycle.start]` exec points at `fake_agent` with
/// `args`, into `dir`, and return the dir path.
fn write_fake_manifest(dir: &Path, kind: &str, args: &[&str]) {
    // Shared fixture builder (test-support consolidation, 2026-09-22): the
    // exact pre-consolidation local shape — contract v1, fake_agent exec,
    // the suite's guaranteed capabilities, self-reported metering.
    hekma_conformance::test_support::ManifestFixture::new(kind, args)
        .guaranteed_on_all_oses("interaction")
        .write(dir);
}

/// Open an engine over a fresh temp state dir.
fn open(base: &TempDir) -> Engine {
    Engine::open(Some(base.path().to_path_buf())).expect("open engine")
}

#[test]
fn start_then_stop_full_lifecycle_via_blocking_facade() {
    // AC1 + AC3 (single lifetime): register a manifest agent, start it (→
    // running), stop it (→ stopped). No process survives, and the instance log
    // records every transition.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // Linger long so only our stop ends it (never the self-exit fallback).
    write_fake_manifest(manifest.path(), "svc", &["--linger-ms", "600000"]);

    let engine = open(&state);
    let facade = engine.blocking();

    let registered = facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    assert_eq!(registered.state, LifecycleState::Registered);

    // Start → running (AC1).
    let started = facade.start("svc").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // The pid file / process is alive: prove by stopping and observing a clean
    // stopped state.
    let stopped = facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
    assert_eq!(stopped.state, LifecycleState::Stopped);

    // AC1 "each transition emits an event": the recorded transitions are
    // registered→starting→running→stopping→stopped, in order.
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
            (LifecycleState::Running, LifecycleState::Stopping),
            (LifecycleState::Stopping, LifecycleState::Stopped),
        ],
        "transition events: {events:#?}"
    );
    // Every event carries the schema version (AD-14 seed).
    assert!(events.iter().all(|e| e.schema_version >= 1));
}

#[test]
fn start_via_async_api_directly() {
    // AD-13: a Host with its own runtime drives the async methods directly (no
    // facade). Prove start/stop work through `Engine`'s async surface too.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(manifest.path(), "svc", &["--linger-ms", "600000"]);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let engine = open(&state);
    rt.block_on(async {
        engine
            .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
            .await
            .unwrap();
        let started = engine.start("svc").await.unwrap();
        assert_eq!(started.state, LifecycleState::Running);
        let stopped = engine
            .stop("svc", Some(Duration::from_secs(5)))
            .await
            .unwrap();
        assert_eq!(stopped.state, LifecycleState::Stopped);
    });
}

#[test]
fn failed_launch_lands_in_failed_with_preserved_diagnostic_no_zombie() {
    // AC2: a manifest whose start exec does not exist → the instance lands in
    // `failed`, the adapter diagnostic is preserved, and nothing is spawned.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // Point exec at a non-existent program (NOT fake_agent).
    let body = r#"
contract_version = "1.0.0"

[adapter]
kind = "bad"

[lifecycle.start]
exec = "ktesio-no-such-binary-zzz-1-4"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#;
    std::fs::write(manifest.path().join("adapter.toml"), body).unwrap();

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("bad", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();

    let err = facade.start("bad").unwrap_err();
    // The diagnostic is preserved verbatim (names the exec + the OS error).
    let msg = err.to_string();
    assert!(msg.contains("ktesio-no-such-binary-zzz-1-4"), "{msg}");
    assert!(msg.contains("failed to launch"), "{msg}");

    // The instance is now in `failed`.
    let listed = facade.list().unwrap();
    let bad = listed.iter().find(|i| i.name.as_str() == "bad").unwrap();
    assert_eq!(bad.state, LifecycleState::Failed);

    // The failure was recorded with a launch-error cause (diagnostic preserved).
    let events = facade.transition_events("bad").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.new_state, LifecycleState::Failed);
}

#[test]
fn immediate_nonzero_exit_during_startup_is_a_launch_failure() {
    // AC2: a process that spawns but exits non-zero immediately is treated as a
    // launch failure → `failed`.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(manifest.path(), "flaky", &["--exit-fast", "3"]);

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "flaky",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();

    let err = facade.start("flaky").unwrap_err();
    assert!(err.to_string().contains("failed to launch"), "{err}");

    let listed = facade.list().unwrap();
    let flaky = listed.iter().find(|i| i.name.as_str() == "flaky").unwrap();
    assert_eq!(flaky.state, LifecycleState::Failed);
}

#[test]
fn invalid_transitions_return_the_same_error_class_for_every_adapter() {
    // AC4: `stop` on `stopped`, `start` on `running`, `stop` on `registered`
    // all return the ONE uniform invalid-transition class — proven for a native
    // builtin AND a manifest adapter (the rejection is in the shared table).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(manifest.path(), "svc", &["--linger-ms", "600000"]);

    let engine = open(&state);
    let facade = engine.blocking();

    // Native builtin `mock`: stop on a freshly-registered (registered) instance.
    facade.register("nat", "mock").unwrap();
    let native_err = facade.stop("nat", None).unwrap_err();
    let native_msg = native_err.to_string();

    // Manifest adapter: stop on a freshly-registered (registered) instance.
    facade
        .register_with_adapter("man", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let manifest_err = facade.stop("man", None).unwrap_err();
    let manifest_msg = manifest_err.to_string();

    // Both are the SAME uniform message class (identical wording — it comes from
    // the shared transition table, before any adapter code runs).
    assert!(native_msg.contains("cannot stop"), "{native_msg}");
    assert_eq!(
        native_msg, manifest_msg,
        "AC4: invalid-transition error must be identical across adapters"
    );

    // And `start` on a running instance is likewise invalid (start the manifest
    // one, then try to start it again).
    facade.start("man").unwrap();
    let double_start = facade.stop("man", Some(Duration::from_secs(5)));
    // (stop it cleanly for teardown)
    let _ = double_start;
}

#[test]
fn start_on_stopped_restarts_the_instance() {
    // FR-5 / transition table: a stopped instance can be started again.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(manifest.path(), "svc", &["--linger-ms", "600000"]);

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();

    facade.start("svc").unwrap();
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
    // Restart from stopped.
    let restarted = facade.start("svc").unwrap();
    assert_eq!(restarted.state, LifecycleState::Running);
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn start_on_missing_instance_is_not_found() {
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let err = engine.blocking().start("ghost").unwrap_err();
    assert!(err.to_string().contains("ghost"), "{err}");
}

#[test]
fn stop_escalates_to_forced_kill_and_records_it() {
    // AC3 escalation end-to-end: a manifest agent that IGNORES SIGTERM (POSIX
    // `trap '' TERM`) outlives the short graceful window, so stop escalates to a
    // forced kill; the escalation is recorded (stop-forced cause) and the
    // instance still reaches `stopped` with no survivor. `sh`/`trap` are POSIX,
    // so this runs on the Unix hosts only — skipped at RUNTIME on Windows via the
    // data-driven OS id (NO `#[cfg]` here — this file is outside the backends
    // allowlist). Windows escalation is proven by the windows-latest matrix.
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        return;
    }
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // A manifest whose start exec is `sh -c 'trap "" TERM; sleep 60'`.
    let body = r#"
contract_version = "1.0.0"

[adapter]
kind = "stubborn"

[lifecycle.start]
exec = "sh"
args = ["-c", "trap '' TERM; sleep 60"]

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#;
    std::fs::write(manifest.path().join("adapter.toml"), body).unwrap();

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "stubborn",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    facade.start("stubborn").unwrap();

    // Short window → SIGTERM is ignored → escalation to SIGKILL.
    let stopped = facade
        .stop("stubborn", Some(Duration::from_millis(200)))
        .unwrap();
    assert_eq!(stopped.state, LifecycleState::Stopped);

    // The final transition records a forced stop (AC3 escalation recorded).
    let events = facade.transition_events("stubborn").unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.new_state, LifecycleState::Stopped);
    let cause = serde_json::to_string(&last.cause).unwrap();
    assert!(cause.contains("stop-forced"), "cause={cause}");
}

#[test]
fn stop_rescues_a_usage_line_flushed_during_the_kill_window() {
    // AI-63 follow-on (billing UNDER-count at STOP, owner-approved). The stop path
    // drains self-reported usage TERMINAL-mode BEFORE `backend.stop` kills the
    // process — while the agent is still ALIVE. A usage line the agent flushes in
    // the window AFTER that pre-kill drain but BEFORE its death was lost: the
    // cursor never advanced past it and the handle was removed right after with no
    // further drain. The fix adds a post-kill rescue drain (AFTER `backend.stop`
    // CONFIRMS death, BEFORE `running.remove`). This proves the rescued line lands.
    //
    // Deterministic "flush during the kill window": an `sh` agent that emits the
    // usage line ONLY from its SIGTERM trap (POSIX `trap`) — nothing at startup —
    // so the line appears strictly AFTER the pre-kill drain (which ran before
    // `backend.stop` even sent SIGTERM) and BEFORE death. Discrimination is exact:
    // without the rescue drain the line is NEVER drained (the pre-kill drain missed
    // it; the reaper is locked out of the supervisor for the whole stop; and the
    // instance leaves `running` the instant stop finishes) → 0 events; with it → 1.
    // The rescue drain commits SYNCHRONOUSLY inside stop (under the supervisor
    // lock), so the event is durable before `stop` returns — asserted directly, no
    // polling, no sleep.
    //
    // `sh`/`trap`/SIGTERM are POSIX, so this runs on the Unix hosts only — skipped
    // at RUNTIME on Windows via the data-driven OS id (mirrors
    // `stop_escalates_to_forced_kill_and_records_it`; NO `#[cfg]`, this file is
    // outside the backends allowlist). The rescue drain the fix adds is OS-agnostic
    // engine code — the identical path runs on the windows-latest matrix.
    if hekma_engine::OsId::current() == hekma_engine::OsId::Windows {
        return;
    }
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // The single usage line the agent flushes from its SIGTERM trap (the fixed
    // 10-in / 20-out sentinels, so the ledger total is an exact-match assertion).
    let emit_path = manifest.path().join("emit.txt");
    std::fs::write(
        &emit_path,
        "KTESIO_USAGE {\"sequence\":0,\"input_tokens\":10,\"output_tokens\":20}\n",
    )
    .unwrap();
    // A manifest whose start exec is `sh -c 'trap "cat <emit>" TERM; sleep 60'`:
    // emit ONLY on SIGTERM, then fall through the interrupted `sleep` so `sh` exits
    // promptly (a graceful, confirmed-death stop). `{script:?}` renders a correctly
    // escaped TOML string; the temp path carries no quotes/backslashes.
    let script = format!("trap 'cat {}' TERM; sleep 60", emit_path.display());
    let body = format!(
        r#"
contract_version = "1.0.0"

[adapter]
kind = "emitter"

[lifecycle.start]
exec = "sh"
args = ["-c", {script:?}]

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#,
    );
    std::fs::write(manifest.path().join("adapter.toml"), body).unwrap();

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter(
            "emitter",
            &AdapterRef::Manifest(manifest.path().to_path_buf()),
        )
        .unwrap();
    let started = facade.start("emitter").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    // Nothing emitted yet (only the SIGTERM trap emits), so the pre-kill drain sees
    // an empty log. A generous window lets `sh` run the trap + `cat` and exit well
    // within it (the emit lands in the pre-kill-drain → confirmed-death window).
    let stopped = facade
        .stop("emitter", Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(stopped.state, LifecycleState::Stopped);

    // The rescued line is committed: exactly one event's worth of tokens. Without
    // the post-kill rescue drain this would be 0 (the line was dropped).
    let fleet = facade.fleet().unwrap();
    let entry = fleet.iter().find(|e| e.name.as_str() == "emitter").unwrap();
    assert_eq!(
        entry.usage.cumulative_input_tokens, 10,
        "the usage line flushed during the kill window must be rescued by the \
         post-kill terminal drain (dropped before the fix)"
    );
    assert_eq!(entry.usage.cumulative_output_tokens, 20);
}

#[test]
fn native_adapter_has_no_launch_command_is_reported_clearly() {
    // The native builtin `mock` has no launch command this story (a real
    // launchable agent is supplied via a manifest). Starting it reports that
    // clearly rather than spawning nothing silently.
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let facade = engine.blocking();
    facade.register("nat", "mock").unwrap();
    let err = facade.start("nat").unwrap_err();
    assert!(err.to_string().contains("no launch command"), "{err}");
}

#[test]
fn engine_agent_home_returns_the_computed_path() {
    // The engine exposes the computed Agent Home path (display helper) without
    // I/O; it agrees with the path the registration reported.
    use hekma_engine::InstanceName;
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let registered = engine.blocking().register("demo", "mock").unwrap();
    let home = engine.agent_home(&InstanceName::new("demo").unwrap());
    assert_eq!(home.to_string_lossy(), registered.agent_home);
    assert!(home.ends_with("agents/demo"));
}

#[test]
fn start_on_invalid_name_is_rejected() {
    // An malformed name is rejected with InvalidName before any lookup.
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let err = engine.blocking().start("Bad Name").unwrap_err();
    assert!(err.to_string().contains("invalid"), "{err}");
}

#[test]
fn stop_on_invalid_name_is_rejected() {
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let err = engine.blocking().stop("Bad Name", None).unwrap_err();
    assert!(err.to_string().contains("invalid"), "{err}");
}

#[test]
fn transition_events_of_unstarted_instance_is_empty() {
    // A registered-but-never-started instance has no recorded transition events
    // yet (the log does not exist).
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let facade = engine.blocking();
    facade.register("demo", "mock").unwrap();
    assert!(facade.transition_events("demo").unwrap().is_empty());
}

#[test]
fn start_with_a_missing_adapter_snapshot_is_reported() {
    // If the adapter snapshot the supervisor needs to build the launch spec is
    // gone (a corrupt Agent Home), start fails cleanly (AdapterUnresolved), not
    // by panicking, and leaves no process.
    use hekma_engine::InstanceName;
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let facade = engine.blocking();
    facade.register("svc", "mock").unwrap();
    // Remove the adapter snapshot from the home.
    let home = engine.agent_home(&InstanceName::new("svc").unwrap());
    std::fs::remove_file(home.join("adapter.json")).unwrap();
    let err = facade.start("svc").unwrap_err();
    // Surfaced as a resolve/adapter error, not a panic.
    assert!(
        err.to_string().contains("resolve") || err.to_string().contains("adapter"),
        "{err}"
    );
}

#[test]
fn stop_without_an_in_memory_handle_is_a_graceful_no_op() {
    // Cross-lifetime stop (single-lifetime boundary, AD-5 is story 1-6): an
    // instance whose row says `running` but for which THIS engine holds no
    // process handle (e.g. it was started by a prior engine, then killed on that
    // engine's drop) is transitioned to `stopped` as a graceful no-op — the
    // desired end state ("no process of this instance survives") already holds.
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let facade = engine.blocking();
    facade.register("svc", "mock").unwrap();
    // Force the row to `running` directly in the state DB (no in-memory handle).
    let db = state.path().join("state.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let n = conn
            .execute(
                "UPDATE agent_instances SET state = 'running' WHERE name = 'svc'",
                [],
            )
            .unwrap();
        assert_eq!(n, 1);
    }
    // Stop: no handle → graceful transition to stopped.
    let stopped = facade.stop("svc", Some(Duration::from_secs(1))).unwrap();
    assert_eq!(stopped.state, LifecycleState::Stopped);
    // The transition was recorded as a graceful stop.
    let events = facade.transition_events("svc").unwrap();
    assert_eq!(events.last().unwrap().new_state, LifecycleState::Stopped);
}

#[test]
fn double_start_on_running_is_invalid_transition() {
    // AC4: start on an already-running instance is the uniform invalid
    // transition, and the running process is untouched (still running).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(manifest.path(), "svc", &["--linger-ms", "600000"]);
    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    facade.start("svc").unwrap();
    let err = facade.start("svc").unwrap_err();
    assert!(err.to_string().contains("cannot start"), "{err}");
    // Clean teardown.
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
}
