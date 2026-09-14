//! Integration tests for `kt agent register | remove | list`.
//!
//! These drive the real `kt` binary (via `CARGO_BIN_EXE_kt`) with
//! `KTESIO_STATE_DIR` pinned to a `TempDir`, so no test ever touches the real
//! user data dir. They assert the CLI contract: exit codes, the Agent Home
//! path on stdout, and diagnostics on stderr.

mod helpers;

use std::path::Path;

use helpers::{run_kt_agent, run_kt_agent_with_env, TestContext};
use ktesio_conformance::test_support::{current_os_key, ManifestFixture};
use ktesio_conformance::uj3;
use ktesio_engine::{Engine, FleetEntry, LifecycleState, UsageView};

/// Path to the SQLite state DB the engine creates under a state base.
fn state_db(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("state.db")
}

/// Seed a `running` state onto an already-registered instance by writing
/// directly into the engine's SQLite DB. This is how the AC5 running-guard is
/// exercised end-to-end before a real supervision core exists (story 1.4).
fn force_state_running(state_dir: &Path, name: &str) {
    let conn = rusqlite::Connection::open(state_db(state_dir)).expect("open state db");
    let affected = conn
        .execute(
            "UPDATE agent_instances SET state = 'running' WHERE name = ?1",
            [name],
        )
        .expect("update state");
    assert_eq!(affected, 1, "expected to update exactly one row");
}

/// Start an instance via a SEPARATE, leaked engine subprocess (crash semantics)
/// so the spawned `fake_agent` SURVIVES the command's exit and can be adopted by
/// the next `kt` invocation (story 1-6). A normal `kt agent start` cleanly drops
/// its engine, which kills the process (the single-lifetime `Drop`), so pause on
/// a later invocation would honestly reconcile the dead-process row to `failed`.
/// To exercise real pause-on-a-LIVE-adopted-instance the process must genuinely
/// outlive its starter — which is exactly the engine-crash case: this re-execs
/// the test binary into `agent_cli_start_helper_subprocess`, which opens an
/// engine, starts the instance, and `std::process::exit`s WITHOUT dropping the
/// engine (no handle Drop → the agent survives and re-parents to init).
///
/// **`_unix` naming convention (fix pass, H4).** Cross-lifetime survival cannot
/// be simulated on Windows (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` kills the child
/// when the helper exits), so tests whose assertions REQUIRE a live process from
/// a previous command's lifetime runtime-`return` there. A runtime early-return
/// is reported by the test runner as **PASSED**, so CI shows green on Windows
/// with zero signal that the assertions never ran. Every such test therefore
/// carries a `_unix` SUFFIX, making the limitation visible in the test list on
/// all three OSes rather than hiding inside the body. Anything those tests guard
/// that is genuinely OS-INDEPENDENT — wire shapes, schema versions, exit codes —
/// must ALSO be asserted by a test that runs everywhere; the `_unix` test is the
/// additional end-to-end proof, never the sole guard. (One exception is stated
/// plainly in the story: exit code `5` has no cross-OS end-to-end path at all,
/// because both routes to it — `pause` and `send` on an `unsupported`
/// declaration — need a genuinely running child. Its full diagnostic→code
/// mapping is pinned cross-OS by the `exit_code.rs` classifier unit tests plus
/// the `map_engine_error`/`map_error` mapper tests in `cli::agent`, and `main`'s
/// wiring of that classifier to the process status is pinned cross-OS by codes
/// `0`/`1`/`2`/`3`/`4`.)
///
/// **Windows-positive siblings (AI-29, story 11-5).** Since story 11-5 the
/// pause family is no longer Unix-only in SILENCE: where a Windows-correct
/// CLI behavior exists it has its own affirmatively-running test
/// (`pause_after_windows_engine_death_reconciles_and_fails_honestly_windows`
/// — engine death kills the child via job-close, and a later `pause`
/// surfaces the honest reconciled-to-`failed` failure, never a fake
/// `paused`), and where a Windows equivalent is genuinely meaningless
/// (guaranteed suspension; exit code `5`'s unsupported-capability route) the
/// skipped test's comment says so.
fn start_via_surviving_engine(state_dir: &Path, name: &str) {
    let exe = std::env::current_exe().expect("test exe");
    let status = std::process::Command::new(exe)
        .args([
            "--exact",
            "agent_cli_start_helper_subprocess",
            "--nocapture",
        ])
        .env("KTESIO_CLI_START_HELPER", name)
        .env("KTESIO_STATE_DIR", state_dir)
        .status()
        .expect("run cli start helper subprocess");
    assert!(
        status.success(),
        "cli start helper subprocess failed: {status}"
    );
}

/// The re-exec entry for [`start_via_surviving_engine`]. When
/// `KTESIO_CLI_START_HELPER` is unset this is a trivial pass. When set, it opens
/// an engine over `KTESIO_STATE_DIR`, starts the named instance, and exits
/// WITHOUT dropping the engine — leaving a surviving, adoptable process.
#[test]
fn agent_cli_start_helper_subprocess() {
    let Ok(name) = std::env::var("KTESIO_CLI_START_HELPER") else {
        return;
    };
    let state = std::path::PathBuf::from(std::env::var("KTESIO_STATE_DIR").unwrap());
    let engine = ktesio_engine::Engine::open(Some(state)).expect("helper engine open");
    engine.blocking().start(&name).expect("helper start");
    // Exit WITHOUT dropping `engine` (crash semantics): the started process
    // survives and re-parents to init, ready for the next command to adopt.
    std::process::exit(0);
}

#[test]
fn register_prints_home_path_and_exits_zero() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    let run = run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(run.success, "register should exit 0; stderr={}", run.stderr);

    // The Agent Home path is printed to stdout and exists on disk.
    let home = state_dir.join("agents").join("demo");
    assert!(
        run.stdout.contains(&home.to_string_lossy().to_string()),
        "stdout should contain the home path; stdout={}",
        run.stdout
    );
    assert!(home.join("config.toml").is_file());
    // The success confirmation is a command result (stdout), consistent with
    // every other `kt` command's `ui::success` usage. AD-12 reserves stderr for
    // diagnostics/notices; a completed-successfully confirmation is not one.
    assert!(run.stdout.contains("Registered"));
    // AC1: the effective per-OS Capability Declaration is surfaced on stdout
    // (the mock declares `pause` and `interaction`).
    assert!(
        run.stdout.contains("Capabilities for demo"),
        "stdout should render capabilities; stdout={}",
        run.stdout
    );
    assert!(run.stdout.contains("pause"), "stdout={}", run.stdout);
    // The adapter snapshot is persisted in the Agent Home.
    assert!(home.join("adapter.json").is_file());
}

#[test]
fn duplicate_registration_exits_nonzero_with_diagnostic() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    let first = run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(first.success);

    // Re-register the same NAME (kind must resolve, so reuse `mock`).
    let second = run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(!second.success, "duplicate should exit non-zero");
    // Diagnostic names the conflict and gives a remediation hint (to stderr).
    assert!(
        second.stderr.contains("already exists"),
        "stderr={}",
        second.stderr
    );
    assert!(second.stderr.contains("kt agent remove demo"));
}

#[test]
fn register_hermes_kind_plumbs_through_the_cli_and_surfaces_in_list() {
    // Review blind-10: engine-API tests prove the hermes builtin resolves, but
    // the CLI arg-plumbing (`--kind hermes` → AdapterRef::Native → engine) is
    // its own surface. This registers the REAL hermes kind at the CLI level —
    // register-only, NO spawn (hermes is never started here) — and asserts the
    // kind survives the round trip through register's stdout confirmation and
    // `list --json`'s machine-readable kind field.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    let run = run_kt_agent(
        &["agent", "register", "gw", "--kind", "hermes"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        run.success,
        "register --kind hermes should exit 0; stderr={}",
        run.stderr
    );
    assert!(run.stdout.contains("Registered"), "stdout={}", run.stdout);
    assert!(
        run.stdout.contains("gw"),
        "the instance name should be echoed; stdout={}",
        run.stdout
    );

    // The engine's builtin table resolved the kind (hermes declares
    // self-reported metering + a non-empty declaration; an unknown kind would
    // have been rejected before any state was written).
    let home = state_dir.join("agents").join("gw");
    assert!(home.join("adapter.json").is_file(), "snapshot persisted");

    // The kind surfaces verbatim in `list --json`.
    let listed = run_kt_agent(&["agent", "list", "--json"], &ctx.project_dir, state_dir);
    assert!(
        listed.success,
        "list --json should exit 0; stderr={}",
        listed.stderr
    );
    let doc: serde_json::Value = serde_json::from_str(&listed.stdout)
        .unwrap_or_else(|e| panic!("stdout not JSON: {e}\n{}", listed.stdout));
    let instances = doc["instances"].as_array().expect("instances array");
    assert_eq!(instances.len(), 1, "{doc}");
    assert_eq!(instances[0]["name"], serde_json::json!("gw"), "{doc}");
    assert_eq!(instances[0]["kind"], serde_json::json!("hermes"), "{doc}");
    assert_eq!(
        instances[0]["metering_source"],
        serde_json::json!("self-reported"),
        "{doc}"
    );
}

#[test]
fn invalid_name_exits_nonzero() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    let run = run_kt_agent(
        &["agent", "register", "Bad_Name", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(!run.success, "invalid name should exit non-zero");
    assert!(run.stderr.contains("Invalid Agent Instance name"));
}

#[test]
fn list_shows_registered_instances() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    // Empty Fleet first.
    let empty = run_kt_agent(&["agent", "list"], &ctx.project_dir, state_dir);
    assert!(empty.success);
    assert!(
        empty.stdout.contains("No Agent Instances") || empty.stderr.contains("No Agent Instances")
    );

    run_kt_agent(
        &["agent", "register", "alpha", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    let listed = run_kt_agent(&["agent", "list"], &ctx.project_dir, state_dir);
    assert!(listed.success);
    assert!(listed.stdout.contains("alpha"), "stdout={}", listed.stdout);
    assert!(listed.stdout.contains("Fleet"));
}

#[test]
fn remove_delete_removes_home() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    let home = state_dir.join("agents").join("demo");
    assert!(home.is_dir());

    let run = run_kt_agent(
        &["agent", "remove", "demo", "--delete"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        run.success,
        "remove --delete should exit 0; stderr={}",
        run.stderr
    );
    assert!(!home.exists(), "home should be gone after --delete");
}

#[test]
fn remove_retain_keeps_home() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    let home = state_dir.join("agents").join("demo");

    // Default (no flag) retains; be explicit here to assert the flag path.
    let run = run_kt_agent(
        &["agent", "remove", "demo", "--retain"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        run.success,
        "remove --retain should exit 0; stderr={}",
        run.stderr
    );
    assert!(home.is_dir(), "home should remain after --retain");
}

#[test]
fn remove_running_without_force_exits_nonzero_and_with_force_succeeds() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    run_kt_agent(
        &["agent", "register", "live", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    // Seed the running state directly (no supervision core yet — AC5 boundary).
    force_state_running(state_dir, "live");

    // Without --force: refused, non-zero, diagnostic to stderr.
    let refused = run_kt_agent(
        &["agent", "remove", "live", "--delete"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !refused.success,
        "running remove without --force should fail"
    );
    assert!(
        refused.stderr.contains("running") && refused.stderr.contains("--force"),
        "stderr={}",
        refused.stderr
    );
    // Instance still present.
    let still = run_kt_agent(&["agent", "list"], &ctx.project_dir, state_dir);
    assert!(still.stdout.contains("live"));

    // With --force: succeeds.
    let forced = run_kt_agent(
        &["agent", "remove", "live", "--delete", "--force"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        forced.success,
        "running remove with --force should exit 0; stderr={}",
        forced.stderr
    );
    let home = state_dir.join("agents").join("live");
    assert!(!home.exists());
}

// ---- Story 1.3: manifest adapters + `kt agent show` ----

/// A complete valid `adapter.toml` for a manifest-adapter directory fixture.
const VALID_MANIFEST: &str = r#"
contract_version = "1.0.0"

[adapter]
kind = "demo-manifest"

[lifecycle.start]
exec = "demo-agent"

[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "best-effort"

[metering]
source = "self-reported"
"#;

/// Write an `adapter.toml` into a fresh subdirectory of `dir` and return it.
///
/// Deliberately NOT consolidated onto the shared
/// `ktesio_conformance::test_support::ManifestFixture` (story 10-1): this
/// writer takes a RAW TOML body, which is the point — the register/show
/// failure-path tests feed it INTENTIONALLY INVALID or bespoke manifests
/// (a missing `[metering]`, a nonexistent `exec`, hand-picked per-OS pause
/// levels the shared presets do not model). A builder that cannot produce
/// broken TOML is exactly what those tests need to stay honest; the shared
/// fixture builder serves the VALID fake_agent shapes (its presets), which
/// this file's `fake_agent_manifest*` wrappers consume.
fn manifest_dir(dir: &Path, body: &str) -> std::path::PathBuf {
    let m = dir.join("manifest-adapter");
    std::fs::create_dir_all(&m).unwrap();
    std::fs::write(m.join("adapter.toml"), body).unwrap();
    m
}

#[test]
fn register_manifest_exits_zero_and_shows_capabilities() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let m = manifest_dir(&ctx.project_dir, VALID_MANIFEST);

    let run = run_kt_agent(
        &["agent", "register", "m", "--manifest", m.to_str().unwrap()],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        run.success,
        "manifest register should exit 0; stderr={}",
        run.stderr
    );
    // The instance kind comes from the manifest's [adapter] kind.
    assert!(
        run.stdout.contains("demo-manifest"),
        "stdout={}",
        run.stdout
    );
    assert!(
        run.stdout.contains("Capabilities for m"),
        "stdout={}",
        run.stdout
    );
    // Home + adapter snapshot exist.
    let home = state_dir.join("agents").join("m");
    assert!(home.join("adapter.json").is_file());
}

#[test]
fn register_invalid_manifest_exits_nonzero_naming_section() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    // Drop [metering] → invalid, naming the section.
    let body = VALID_MANIFEST.replace("[metering]\nsource = \"self-reported\"\n", "");
    let m = manifest_dir(&ctx.project_dir, &body);

    let run = run_kt_agent(
        &["agent", "register", "m", "--manifest", m.to_str().unwrap()],
        &ctx.project_dir,
        state_dir,
    );
    assert!(!run.success, "invalid manifest should exit non-zero");
    assert!(
        run.stderr.contains("[metering]"),
        "diagnostic should name the section; stderr={}",
        run.stderr
    );
    // No partial state.
    assert!(!state_dir.join("agents").join("m").exists());
}

#[test]
fn register_manifest_not_found_exits_nonzero() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let missing = ctx.project_dir.join("no-such-dir");

    let run = run_kt_agent(
        &[
            "agent",
            "register",
            "m",
            "--manifest",
            missing.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(!run.success, "missing manifest should exit non-zero");
    assert!(
        run.stderr.contains("adapter.toml") || run.stderr.contains("No adapter.toml"),
        "stderr={}",
        run.stderr
    );
}

#[test]
fn register_unknown_kind_exits_nonzero() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    let run = run_kt_agent(
        &["agent", "register", "x", "--kind", "no-such-kind"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(!run.success, "unknown kind should exit non-zero");
    assert!(
        run.stderr.contains("Unknown adapter kind"),
        "stderr={}",
        run.stderr
    );
    assert!(!state_dir.join("agents").join("x").exists());
}

#[test]
fn register_requires_kind_or_manifest() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    // Neither --kind nor --manifest → clap rejects before the engine runs.
    let run = run_kt_agent(&["agent", "register", "x"], &ctx.project_dir, state_dir);
    assert!(!run.success, "register with no adapter flag should fail");
}

#[test]
fn show_renders_effective_capabilities() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    let show = run_kt_agent(&["agent", "show", "demo"], &ctx.project_dir, state_dir);
    assert!(show.success, "show should exit 0; stderr={}", show.stderr);
    assert!(
        show.stdout.contains("Capabilities for demo"),
        "stdout={}",
        show.stdout
    );
    assert!(show.stdout.contains("pause"), "stdout={}", show.stdout);
}

#[test]
fn show_unknown_instance_exits_nonzero() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    let show = run_kt_agent(&["agent", "show", "ghost"], &ctx.project_dir, state_dir);
    assert!(!show.success, "show of a missing instance should fail");
    assert!(
        show.stderr.contains("No Agent Instance named 'ghost'"),
        "stderr={}",
        show.stderr
    );
}

// ---- Story 1.4: `kt agent start` / `kt agent stop` ----

/// Write a manifest whose `[lifecycle.start]` exec points at `fake_agent`,
/// interaction `guaranteed` on all three OSes. The TOML body is the SHARED
/// builder's `fake_agent` preset (story 10-1 —
/// `ktesio_conformance::test_support` states the manifest shape ONCE); this
/// wrapper only keeps the fixture's `fake-agent-adapter` subdirectory name.
/// The locator behind it is the SHARED `ktesio_conformance::fake_agent_bin`:
/// the running kt integration-test binary resolves its own
/// `target/<profile>/deps/` sibling — the same profile dir the old local
/// `CARGO_BIN_EXE_kt`-anchored copy named — so that copy was duplication,
/// not a distinct seam (the `CARGO_BIN_EXE_kt` seam for RUNNING kt is
/// separate and stays).
fn fake_agent_manifest(dir: &Path, args: &[&str]) -> std::path::PathBuf {
    let m = dir.join("fake-agent-adapter");
    ManifestFixture::fake_agent("fake", args).write(&m)
}

#[test]
fn start_prints_running_state_and_exits_zero() {
    // AC1 at the CLI: register a manifest agent, start it → the new state
    // `running` is printed to stdout and the exit code is 0.
    //
    // NOTE (single-lifetime boundary): each `kt` invocation is its own engine
    // lifetime; the started process is cleaned up on exit (kill-on-drop). A
    // separate `kt agent stop` cannot re-attach to it (orphan adoption is story
    // 1-6). This test asserts the START contract; the full start→stop→no-survivor
    // proof lives in the engine's single-lifetime lifecycle integration test.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let m = fake_agent_manifest(&ctx.project_dir, &["--linger-ms", "600000"]);

    run_kt_agent(
        &[
            "agent",
            "register",
            "svc",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    let run = run_kt_agent(&["agent", "start", "svc"], &ctx.project_dir, state_dir);
    assert!(run.success, "start should exit 0; stderr={}", run.stderr);
    assert!(run.stdout.contains("running"), "stdout={}", run.stdout);
    assert!(run.stdout.contains("Started"), "stdout={}", run.stdout);
}

#[test]
fn start_prints_single_lifetime_notice_to_stderr_only() {
    // LOW-1: the success path is honest about single-lifetime supervision — a
    // standalone `kt agent start` kills the agent when the CLI exits cleanly, and
    // durable supervision across SEPARATE CLI invocations is future work (story
    // 1-6 delivered crash recovery, NOT clean-exit cross-command survival). That
    // caveat is printed as a one-line NOTICE to STDERR (AD-12: results → stdout,
    // notices → stderr), and the stdout result line (`running`) is UNCHANGED.
    // This asserts both halves so a future change that either drops the notice or
    // leaks it onto stdout is caught.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let m = fake_agent_manifest(&ctx.project_dir, &["--linger-ms", "600000"]);

    run_kt_agent(
        &[
            "agent",
            "register",
            "svc",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    let run = run_kt_agent(&["agent", "start", "svc"], &ctx.project_dir, state_dir);
    assert!(run.success, "start should exit 0; stderr={}", run.stderr);
    // stdout result line is unchanged (still shows `running`).
    assert!(run.stdout.contains("running"), "stdout={}", run.stdout);
    // The notice is on stderr and states the honest boundary (supervised only for
    // this engine session; cross-invocation durability is future work). It must
    // NOT promise cross-CLI durable supervision as delivered.
    assert!(
        run.stderr
            .contains("supervised only for this engine session"),
        "single-lifetime notice must go to stderr; stderr={}",
        run.stderr
    );
    assert!(
        run.stderr.contains("future work"),
        "notice must state durable cross-invocation supervision is future work; stderr={}",
        run.stderr
    );
    // It must NOT claim durable cross-CLI supervision arrives with 1-6 (that was
    // the false promise this fix removes).
    assert!(
        !run.stderr.contains("across CLI invocations arrives"),
        "notice must not promise cross-CLI durable supervision as delivered; stderr={}",
        run.stderr
    );
    // The notice must NOT leak onto stdout (AD-12: stdout is the result only).
    assert!(
        !run.stdout
            .contains("supervised only for this engine session"),
        "notice must not appear on stdout; stdout={}",
        run.stdout
    );
}

#[test]
fn start_missing_exec_lands_failed_with_preserved_diagnostic() {
    // AC2 at the CLI: a manifest whose start exec does not exist → non-zero
    // exit, the diagnostic is preserved on stderr, and the instance is `failed`.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let m = ctx.project_dir.join("bad-adapter");
    std::fs::create_dir_all(&m).unwrap();
    std::fs::write(
        m.join("adapter.toml"),
        r#"
contract_version = "1.0.0"
[adapter]
kind = "bad"
[lifecycle.start]
exec = "ktesio-no-such-binary-cli-1-4"
[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"
[metering]
source = "self-reported"
"#,
    )
    .unwrap();

    run_kt_agent(
        &[
            "agent",
            "register",
            "bad",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    let run = run_kt_agent(&["agent", "start", "bad"], &ctx.project_dir, state_dir);
    assert!(!run.success, "start of a bad exec should exit non-zero");
    assert!(
        run.stderr.contains("failed to launch"),
        "stderr={}",
        run.stderr
    );
    assert!(
        run.stderr.contains("ktesio-no-such-binary-cli-1-4"),
        "diagnostic preserved; stderr={}",
        run.stderr
    );
    // The instance is now `failed`.
    let list = run_kt_agent(&["agent", "list"], &ctx.project_dir, state_dir);
    assert!(list.stdout.contains("failed"), "stdout={}", list.stdout);
}

#[test]
fn stop_on_stopped_returns_uniform_invalid_transition() {
    // AC4 at the CLI: `stop` on a freshly-registered (registered) instance —
    // never started — returns the uniform invalid-transition diagnostic and a
    // non-zero exit, identical wording regardless of adapter kind.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    // Native builtin.
    run_kt_agent(
        &["agent", "register", "nat", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    let run = run_kt_agent(&["agent", "stop", "nat"], &ctx.project_dir, state_dir);
    assert!(!run.success, "stop on registered should exit non-zero");
    assert!(
        run.stderr.contains("cannot stop"),
        "uniform invalid-transition; stderr={}",
        run.stderr
    );
}

#[test]
fn start_unknown_instance_exits_nonzero() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let run = run_kt_agent(&["agent", "start", "ghost"], &ctx.project_dir, state_dir);
    assert!(!run.success, "start of a missing instance should fail");
    assert!(run.stderr.contains("ghost"), "stderr={}", run.stderr);
}

#[test]
fn stop_accepts_timeout_flag() {
    // The `--timeout <secs>` flag parses and drives the graceful window. Stop on
    // a registered instance still rejects (AC4) but proves the flag is accepted.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "svc", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    let run = run_kt_agent(
        &["agent", "stop", "svc", "--timeout", "5"],
        &ctx.project_dir,
        state_dir,
    );
    // Registered → stop is invalid, but the flag parsed (no clap error).
    assert!(run.stderr.contains("cannot stop"), "stderr={}", run.stderr);
}

// ---- Story 1.5: `kt agent pause` / `kt agent resume` (AC6) ----

/// Write a `fake_agent` manifest whose CURRENT-OS pause level is `pause_level`
/// (interaction `guaranteed` on all three OSes). The TOML body is the SHARED
/// builder's single-OS capability entry + the cross-OS interaction guarantee
/// (story 10-1); this wrapper keeps the fixture's `pause-adapter` subdir name.
fn fake_agent_manifest_with_pause(
    dir: &Path,
    args: &[&str],
    pause_level: &str,
) -> std::path::PathBuf {
    let m = dir.join("pause-adapter");
    ManifestFixture::new("fake", args)
        .capability_on_os("pause", current_os_key(), pause_level)
        .guaranteed_on_all_oses("interaction")
        .write(&m)
}

#[test]
fn pause_prints_paused_state_and_exits_zero_guaranteed_unix() {
    // AC6 + AC1 at the CLI (Unix guaranteed): `kt agent pause` on a genuinely
    // LIVE instance prints the new state `paused` to stdout with exit 0 and NO
    // best-effort qualifier. Runtime-skip on Windows (guaranteed pause is
    // Unix-only); NO cfg — data-driven skip.
    //
    // Per-OS honesty note (AI-29, story 11-5): there is NO Windows sibling
    // for THIS test because a guaranteed suspension does not exist on Windows
    // — the backend's cooperative pause is best-effort by design (AD-4), so
    // "pause a live instance and get an unqualified `paused`" is a Unix-only
    // guarantee by definition, not a gap. The Windows-correct CLI behavior
    // that DOES exist in this family — engine death takes the child, and a
    // later pause fails honestly — is asserted by
    // `pause_after_windows_engine_death_reconciles_and_fails_honestly_windows`.
    //
    // NOTE (single-lifetime CLI boundary, story 1-6): each `kt` command is a
    // short-lived engine whose handle Drop kills the process on the command's
    // clean exit (the story-1-4 single-lifetime safety net; durable
    // cross-invocation supervision remains future work — orphan ADOPTION here
    // covers the engine-CRASH case, proven in `tests/adoption.rs`). So this test
    // proves the pause command's CLI WIRING against a live adopted instance; it
    // does NOT chain a follow-up `kt agent resume` (the paused process does not
    // survive the pause command's clean drop). The pause/resume SEMANTICS —
    // including resume after a real SIGSTOP within one engine lifetime — are
    // covered by the engine integration tests in `tests/pause.rs`.
    if std::env::consts::OS == "windows" {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let m = fake_agent_manifest_with_pause(
        &ctx.project_dir,
        &["--heartbeat-ms", "50", "--linger-ms", "600000"],
        "guaranteed",
    );
    run_kt_agent(
        &[
            "agent",
            "register",
            "svc",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    // Start via a surviving (crashed-engine) subprocess so the process is
    // genuinely LIVE when the pause command adopts it (story 1-6). A plain
    // `kt agent start` would kill it on the command's clean engine drop.
    start_via_surviving_engine(state_dir, "svc");

    let paused = run_kt_agent(&["agent", "pause", "svc"], &ctx.project_dir, state_dir);
    assert!(
        paused.success,
        "guaranteed pause should exit 0; stderr={}",
        paused.stderr
    );
    assert!(paused.stdout.contains("paused"), "stdout={}", paused.stdout);
    assert!(paused.stdout.contains("Paused"), "stdout={}", paused.stdout);
    // A guaranteed pause emits NO best-effort qualifier on stderr.
    assert!(
        !paused.stderr.contains("best-effort"),
        "guaranteed pause must not print a best-effort note; stderr={}",
        paused.stderr
    );

    // Teardown: the pause command's clean drop already killed the SIGSTOP'd
    // process; a `stop` here settles the row (idempotent, no survivor).
    run_kt_agent(&["agent", "stop", "svc"], &ctx.project_dir, state_dir);
}

#[test]
fn pause_after_windows_engine_death_reconciles_and_fails_honestly_windows() {
    // AI-29 (story 11-5) — the Windows-POSITIVE sibling of the two Unix pause
    // tests in this family, asserting the CORRECT Windows CLI semantics. On
    // Windows the surviving-engine helper's child is killed the moment the
    // helper exits (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE — cross-lifetime
    // survival genuinely cannot be simulated), so the one Windows-real pause
    // scenario is the one that follows an engine death: the `kt agent pause`
    // command opens an engine over the state dir, adoption finds the gone
    // process behind the `running` row and reconciles it to `failed` (AI-8),
    // and the pause then fails fast with the uniform invalid-transition
    // diagnostic — never a fabricated `paused` on stdout. This is the
    // end-to-end proof that a Windows pause command can never silently
    // "pause" an instance whose process no longer exists.
    if ktesio_engine::OsId::current() != ktesio_engine::OsId::Windows {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let m =
        fake_agent_manifest_with_pause(&ctx.project_dir, &["--linger-ms", "600000"], "best-effort");
    run_kt_agent(
        &["agent", "register", "be", "--manifest", m.to_str().unwrap()],
        &ctx.project_dir,
        state_dir,
    );
    // Start via the surviving-engine helper; the helper's exit kills the
    // child (job close), leaving a `running` record whose process is gone.
    start_via_surviving_engine(state_dir, "be");

    // Deterministic gate (no OS-cfg — data-driven, this body only runs on
    // Windows): the job-close kill is ASYNCHRONOUS relative to the helper's
    // exit, so wait until the announced agent pid is actually gone before
    // invoking pause — otherwise a still-dying process could be adopted and
    // "paused" best-effort, racing the assertion. Mirrors adoption.rs's
    // `wait_until_gone` discipline (tasklist is the Windows liveness probe).
    let log = state_dir
        .join("agents")
        .join("be")
        .join("logs")
        .join("agent.log");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let pid = loop {
        if let Ok(contents) = std::fs::read_to_string(&log) {
            if let Some(line) = contents.lines().find(|l| l.contains("ready pid=")) {
                if let Some(idx) = line.find("pid=") {
                    if let Ok(pid) = line[idx + 4..].trim().parse::<u32>() {
                        break pid;
                    }
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the agent pid was never announced in {}",
            log.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    loop {
        let alive = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(true);
        if !alive {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the engine-death job-close kill never completed for pid {pid}"
        );
        std::thread::sleep(std::time::Duration::from_millis(30));
    }

    // The pause command must surface the honest reconciled-to-`failed`
    // invalid transition, on stderr, with a non-zero exit.
    let paused = run_kt_agent(&["agent", "pause", "be"], &ctx.project_dir, state_dir);
    assert!(
        !paused.success,
        "pause after the reconciled engine death must exit non-zero; stdout={}",
        paused.stdout
    );
    assert!(
        paused.stderr.contains("cannot pause"),
        "the uniform invalid-transition diagnostic must be on stderr; stderr={}",
        paused.stderr
    );
    // NEVER a fabricated success: no `paused` result line on stdout.
    assert!(
        !paused.stdout.contains("Paused"),
        "pause must not claim success for a reconciled-dead instance; stdout={}",
        paused.stdout
    );
}

#[test]
fn pause_best_effort_prints_qualifier_note_to_stderr_only_unix() {
    // Runtime-skip on Windows (data-driven OS id, NO `#[cfg]` — this file is
    // outside the backends allowlist). This test drives the story-1-6 cross-
    // process adoption harness (`start_via_surviving_engine`): a subprocess
    // starts the agent and exits WITHOUT a graceful stop so the child re-parents
    // and survives, then a separate `kt` command adopts it live. That survival
    // relies on Unix re-parenting to init; on Windows JOB_OBJECT_LIMIT_KILL_ON_
    // JOB_CLOSE kills the child when the helper exits, so the next `Engine::open`
    // adoption reconciles the row to `failed` and pause can't run. Cross-lifetime
    // survival genuinely can't be simulated on Windows (consistent with the
    // engine's documented single-lifetime behavior); the pause/resume SEMANTICS
    // are fully covered on Windows by `crates/ktesio-engine/tests/pause.rs`, and
    // the Windows-correct CLI sibling in this family — the honest
    // reconciled-to-`failed` pause failure after an engine death — runs on the
    // Windows leg
    // (`pause_after_windows_engine_death_reconciles_and_fails_honestly_windows`).
    if ktesio_engine::OsId::current() == ktesio_engine::OsId::Windows {
        return;
    }
    // AC2 + AC6 at the CLI: a best-effort pause prints the new state `paused` to
    // STDOUT and a VISIBLE qualifier NOTE to STDERR (never silent, never on
    // stdout). Mirrors the LOW-1 stdout/stderr-discipline assertion from 1-4.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let m =
        fake_agent_manifest_with_pause(&ctx.project_dir, &["--linger-ms", "600000"], "best-effort");
    run_kt_agent(
        &["agent", "register", "be", "--manifest", m.to_str().unwrap()],
        &ctx.project_dir,
        state_dir,
    );
    // Start via a surviving (crashed-engine) subprocess so the process is live
    // when pause adopts it (story 1-6); a plain `kt agent start` kills it on exit.
    start_via_surviving_engine(state_dir, "be");

    let paused = run_kt_agent(&["agent", "pause", "be"], &ctx.project_dir, state_dir);
    assert!(
        paused.success,
        "best-effort pause should exit 0; stderr={}",
        paused.stderr
    );
    // Result line on stdout.
    assert!(paused.stdout.contains("paused"), "stdout={}", paused.stdout);
    // The qualifier note is on STDERR and names best-effort.
    assert!(
        paused.stderr.contains("best-effort"),
        "best-effort qualifier must be on stderr; stderr={}",
        paused.stderr
    );
    // The qualifier must NOT leak onto stdout (AD-12: stdout is the result only).
    assert!(
        !paused.stdout.contains("best-effort"),
        "qualifier must not appear on stdout; stdout={}",
        paused.stdout
    );

    // Teardown.
    run_kt_agent(&["agent", "stop", "be"], &ctx.project_dir, state_dir);
}

#[test]
fn pause_unsupported_exits_nonzero_quoting_the_declaration_unix() {
    // Runtime-skip on Windows (data-driven OS id, NO `#[cfg]` — this file is
    // outside the backends allowlist). Like the best-effort case above, this test
    // relies on the story-1-6 cross-process adoption harness
    // (`start_via_surviving_engine`) to make the instance genuinely `running`
    // before pause runs. On Windows JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE kills the
    // survivor when the helper subprocess exits, so the next `Engine::open`
    // reconciles the row to `failed` and pause fails with a reconciled-to-failed
    // error instead of the intended UNSUPPORTED diagnostic. Cross-lifetime
    // survival can't be simulated on Windows; the pause semantics (including the
    // unsupported projection) are covered by `crates/ktesio-engine/tests/pause.rs`,
    // and the family's Windows-correct CLI sibling runs on the Windows leg
    // (`pause_after_windows_engine_death_reconciles_and_fails_honestly_windows`).
    // Per-OS honesty note (AI-29, story 11-5): there is deliberately NO Windows
    // sibling for THIS test specifically — exit code `5` requires a genuinely
    // running child with an unsupported pause declaration, a combination no
    // Windows CLI invocation can reach; the diagnostic→code mapping stays pinned
    // cross-OS by the `exit_code.rs` classifier unit tests (the file-level
    // convention note above).
    if ktesio_engine::OsId::current() == ktesio_engine::OsId::Windows {
        return;
    }
    // AC3 + AC6 at the CLI: a pause that is `unsupported` on this OS fails fast
    // with a non-zero exit and a diagnostic (on STDERR) that QUOTES the
    // declaration (names pause, the OS, the level) and points at `kt agent show`.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    // Declare pause only for an OS that is NOT the current one → current-OS
    // projection is Unsupported.
    let other = if std::env::consts::OS == "windows" {
        "linux"
    } else {
        "windows"
    };
    let m = fake_agent_manifest_with_pause(&ctx.project_dir, &["--linger-ms", "600000"], "ignored");
    // Overwrite the manifest so pause is declared ONLY for the other OS
    // (the SHARED builder's single-OS capability entry, story 10-1).
    ManifestFixture::new("fake", &["--linger-ms", "600000"])
        .capability_on_os("pause", other, "guaranteed")
        .guaranteed_on_all_oses("interaction")
        .write(&m);

    run_kt_agent(
        &["agent", "register", "un", "--manifest", m.to_str().unwrap()],
        &ctx.project_dir,
        state_dir,
    );
    // Start via a surviving (crashed-engine) subprocess so the instance is
    // genuinely `running` (adopted) when pause runs — so pause fails fast with
    // the UNSUPPORTED diagnostic, not a reconciled-to-failed transition error.
    start_via_surviving_engine(state_dir, "un");

    let paused = run_kt_agent(&["agent", "pause", "un"], &ctx.project_dir, state_dir);
    assert!(
        !paused.success,
        "unsupported pause must exit non-zero; stdout={}",
        paused.stdout
    );
    // Story 4-3 (DC-5): an unsupported capability is exit code 5 specifically —
    // the end-to-end proof for the one exit class that needs a genuinely
    // running instance to reach (the full diagnostic→code mapping is pinned
    // cross-OS by the `exit_code` classifier unit tests).
    assert_eq!(
        paused.code,
        Some(5),
        "unsupported capability must exit 5; stderr={}",
        paused.stderr
    );
    assert!(
        paused.stderr.contains("cannot pause"),
        "stderr must quote the declaration; stderr={}",
        paused.stderr
    );
    assert!(
        paused.stderr.contains("unsupported"),
        "stderr must name the level; stderr={}",
        paused.stderr
    );
    assert!(
        paused.stderr.contains("kt agent show un"),
        "stderr must point at kt agent show; stderr={}",
        paused.stderr
    );
    // Fail-fast made NO transition to `paused`: the pause command exited
    // non-zero WITHOUT persisting a pause. (We do not re-check the state via a
    // follow-up `kt` command here: each command's clean engine drop kills the
    // adopted process, and the next command's honest adoption would then
    // reconcile the gone process to `failed` — a single-lifetime CLI artifact,
    // NOT a pause side effect. The no-persist guarantee of the unsupported
    // fail-fast is proven at the engine level in `tests/pause.rs`.)

    // Teardown.
    run_kt_agent(&["agent", "stop", "un"], &ctx.project_dir, state_dir);
}

#[test]
fn pause_on_registered_returns_uniform_invalid_transition() {
    // AC4 at the CLI: pause on a registered (never started) instance returns the
    // uniform invalid-transition diagnostic and a non-zero exit.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "nat", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    let run = run_kt_agent(&["agent", "pause", "nat"], &ctx.project_dir, state_dir);
    assert!(!run.success, "pause on registered should exit non-zero");
    assert!(
        run.stderr.contains("cannot pause"),
        "uniform invalid-transition; stderr={}",
        run.stderr
    );
}

// ---- Story 4-1: `kt agent send <name> <text>` (AC-A, AC-B, AC-C) ----

/// Write a `fake_agent` manifest whose CURRENT-OS interaction level is
/// `interaction_level` (mirrors `fake_agent_manifest_with_pause`; NO pause
/// declaration). The TOML body is the SHARED builder's single-OS capability
/// entry (story 10-1); this wrapper keeps the fixture's `send-adapter` name.
fn fake_agent_manifest_with_interaction(
    dir: &Path,
    args: &[&str],
    interaction_level: &str,
) -> std::path::PathBuf {
    let m = dir.join("send-adapter");
    ManifestFixture::new("fake", args)
        .capability_on_os("interaction", current_os_key(), interaction_level)
        .write(&m)
}

/// The agent.log path inside an instance's Agent Home.
fn agent_log_path(state_dir: &Path, name: &str) -> std::path::PathBuf {
    state_dir
        .join("agents")
        .join(name)
        .join("logs")
        .join("agent.log")
}

#[test]
fn send_on_an_adopted_instance_exits_nonzero_with_interaction_unavailable_unix() {
    // DEVIATION FROM THE STORY FILE (documented; see the story's Dev Agent
    // Record / Completion Notes and the PR description for the full
    // rationale): Task 8's first bullet describes this test asserting exit 0
    // + delivered input, mirroring
    // `pause_prints_paused_state_and_exits_zero_guaranteed_unix` via
    // `start_via_surviving_engine`. That harness works for pause (guaranteed
    // pause only needs the pgid/PID, which adoption fully restores), but for
    // `send` it is structurally impossible to succeed: `start_via_surviving_engine`
    // starts the agent in a SEPARATE helper subprocess that exits without
    // dropping its engine, so the SEPARATE `kt agent send` invocation this
    // test drives NECESSARILY reaches the instance only via `adopt_orphans`
    // — and an adopted handle NEVER carries a recoverable stdin pipe (AC-D,
    // Task 1's `adopt()`), on Unix or Windows alike (this is not an OS
    // difference; a pipe file descriptor is process-local and cannot survive
    // a re-parent to init the way a bare pid/pgid does). Running the test AS
    // LITERALLY DESCRIBED reproducibly fails with EXACTLY the
    // `InteractionUnavailable` error AC-D mandates — confirming the
    // implementation is correct and the story's test-outcome description
    // does not account for send's pipe-vs-pid distinction (spelled out in
    // the story's OWN Dev Notes: "pause/stop/poll do not have this problem
    // ... while send needs an actual open file descriptor this engine
    // session holds"). So this test instead proves the CLI-level surfacing
    // of AC-D's honest failure (genuinely new coverage: the
    // `AgentInteractionUnavailable` diagnostic's rendering through the real
    // `kt` binary, which no other test exercises) — never
    // `CapabilityUnsupported`, never a silent success, and NO input
    // delivered. AC-A's single-session happy path (exit 0 + delivered input)
    // is fully proven at the engine level by
    // `send_input_delivers_text_to_a_running_manifest_adapter_agent` in
    // `crates/ktesio-engine/tests/interaction.rs`, which does not cross a
    // process boundary.
    //
    // Runtime-skip on Windows: `start_via_surviving_engine`'s cross-lifetime
    // survival needs Unix re-parenting to init (`JOB_OBJECT_LIMIT_KILL_ON_
    // JOB_CLOSE` kills the child on Windows when the helper exits) — the SAME
    // reason the pause CLI tests skip Windows. NO `#[cfg]` (data-driven; this
    // file is outside the backends allowlist).
    if ktesio_engine::OsId::current() == ktesio_engine::OsId::Windows {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let m = fake_agent_manifest_with_interaction(
        &ctx.project_dir,
        &["--echo-stdin", "--linger-ms", "600000"],
        "guaranteed",
    );
    run_kt_agent(
        &[
            "agent",
            "register",
            "svc",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    // Start via a surviving (crashed-engine) subprocess so the process is
    // genuinely LIVE when the send command's SEPARATE engine adopts it.
    start_via_surviving_engine(state_dir, "svc");

    let sent = run_kt_agent(
        &["agent", "send", "svc", "hello"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !sent.success,
        "send on an adopted instance must exit non-zero (no recoverable pipe); stdout={}",
        sent.stdout
    );
    // Story 4-3 (DC-5), strengthened in the fix pass (H1): assert the NUMBER, not
    // merely "non-zero". `AgentInteractionUnavailable` is documented as exit 5, and
    // a bare `!success` would have passed just as happily on a 1 — leaving the
    // wiring from this path to its documented code unpinned end-to-end.
    assert_eq!(
        sent.code,
        Some(5),
        "an unavailable interaction channel must exit 5; stderr={}",
        sent.stderr
    );
    assert!(
        sent.stderr.contains("cannot receive input"),
        "stderr must state the honest InteractionUnavailable cause; stderr={}",
        sent.stderr
    );
    assert!(
        sent.stderr.contains("svc"),
        "stderr must name the instance; stderr={}",
        sent.stderr
    );
    // Never misattributed to CapabilityUnsupported — the adapter's
    // declaration is truthful; it is this session's reach that is limited.
    assert!(
        !sent.stderr.contains("unsupported") && !sent.stderr.contains("declares"),
        "must never be misattributed to CapabilityUnsupported: stderr={}",
        sent.stderr
    );

    // Confirm this genuinely exercised the ADOPTED-and-running path (not some
    // other failure): the persisted row is `running`.
    let conn = rusqlite::Connection::open(state_db(state_dir)).unwrap();
    let persisted_state: String = conn
        .query_row(
            "SELECT state FROM agent_instances WHERE name = 'svc'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        persisted_state, "running",
        "the instance must genuinely be adopted as running for this to be AC-D, not some other failure"
    );
    drop(conn);

    // No input was delivered (a failed send must deliver NOTHING).
    let contents = std::fs::read_to_string(agent_log_path(state_dir, "svc")).unwrap_or_default();
    assert!(
        !contents.lines().any(|l| l.starts_with("stdin:")),
        "a failed send must deliver no input: {contents:?}"
    );

    // Teardown: stop the adopted process so no orphan remains.
    run_kt_agent(&["agent", "stop", "svc"], &ctx.project_dir, state_dir);
}

#[test]
fn send_unsupported_exits_nonzero_quoting_the_declaration_unix() {
    // AC-B at the CLI: `send` on an instance whose interaction is
    // `unsupported` on this OS fails fast with a non-zero exit and a
    // diagnostic (stderr) quoting the declaration. Same
    // `start_via_surviving_engine` requirement as pause's identical test — an
    // unsupported-but-DB-hacked `force_state_running` would not survive
    // `adopt_orphans`' reconciliation (see that test's own comment on why the
    // heavier harness is required even for the fail-fast path).
    if ktesio_engine::OsId::current() == ktesio_engine::OsId::Windows {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    // Declare interaction only for an OS that is NOT the current one → the
    // current-OS projection is Unsupported.
    let other = if std::env::consts::OS == "windows" {
        "linux"
    } else {
        "windows"
    };
    let m = fake_agent_manifest_with_interaction(
        &ctx.project_dir,
        &["--echo-stdin", "--linger-ms", "600000"],
        "ignored",
    );
    // Overwrite the manifest so interaction is declared ONLY for the other
    // OS (the SHARED builder's single-OS capability entry, story 10-1).
    ManifestFixture::new("fake", &["--echo-stdin", "--linger-ms", "600000"])
        .capability_on_os("interaction", other, "guaranteed")
        .write(&m);

    run_kt_agent(
        &["agent", "register", "un", "--manifest", m.to_str().unwrap()],
        &ctx.project_dir,
        state_dir,
    );
    // Start via a surviving (crashed-engine) subprocess so the instance is
    // genuinely `running` (adopted) when send runs — so send fails fast with
    // the UNSUPPORTED diagnostic, not a reconciled-to-failed transition error.
    start_via_surviving_engine(state_dir, "un");

    let sent = run_kt_agent(
        &["agent", "send", "un", "hello"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !sent.success,
        "unsupported send must exit non-zero; stdout={}",
        sent.stdout
    );
    // Story 4-3 (DC-5), added in the fix pass (H1): the SECOND end-to-end proof
    // that an unsupported capability is exit 5 specifically (the first is
    // `pause_unsupported_exits_nonzero_quoting_the_declaration_unix`).
    assert_eq!(
        sent.code,
        Some(5),
        "an unsupported capability must exit 5; stderr={}",
        sent.stderr
    );
    assert!(
        sent.stderr.contains("cannot interaction"),
        "stderr must quote the declaration; stderr={}",
        sent.stderr
    );
    assert!(
        sent.stderr.contains("unsupported"),
        "stderr must name the level; stderr={}",
        sent.stderr
    );
    assert!(
        sent.stderr.contains("kt agent show un"),
        "stderr must point at kt agent show; stderr={}",
        sent.stderr
    );

    // Fail-fast attempted NO I/O: the captured log gained no `stdin:` line.
    let contents = std::fs::read_to_string(agent_log_path(state_dir, "un")).unwrap_or_default();
    assert!(
        !contents.lines().any(|l| l.starts_with("stdin:")),
        "no I/O may be attempted on the Unsupported fail-fast path: {contents:?}"
    );

    // Teardown.
    run_kt_agent(&["agent", "stop", "un"], &ctx.project_dir, state_dir);
}

#[test]
fn send_on_registered_instance_is_not_running() {
    // AC-C at the CLI: `send` on a registered (never started) instance fails
    // with a non-zero exit and a diagnostic naming the current state — the
    // LIGHTER path suffices here (no live process needed at all): the
    // `NotRunning` check fires before anything process-related, mirroring
    // `pause_on_registered_returns_uniform_invalid_transition`.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "nat", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    let run = run_kt_agent(
        &["agent", "send", "nat", "hello"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(!run.success, "send on registered should exit non-zero");
    assert!(
        run.stderr.contains("not running"),
        "stderr should name the not-running state; stderr={}",
        run.stderr
    );
    assert!(
        run.stderr.contains("nat"),
        "stderr should name the instance; stderr={}",
        run.stderr
    );
}

// ---- Fix pass (review of #79): M1 (hyphen-safe text / --help interception)
// and M2 (paused-instance remediation) ----

#[test]
fn send_text_that_looks_like_help_flag_is_sent_literally_not_intercepted_as_cli_help() {
    // M1 fix: `text` is hyphen-safe (`allow_hyphen_values`) and `send`'s own
    // `--help`/`-h` is disabled (`disable_help_flag`), so a text value of
    // "--help" is delivered as literal input, never silently intercepted as
    // the CLI's OWN help — which used to print help and exit 0 WITHOUT
    // sending anything, so a caller checking only the exit code would
    // wrongly believe the send succeeded.
    //
    // No instance named "ghost" is registered, so the real send logic must
    // reach the ordinary NotFound diagnostic — proving "--help" was routed
    // as the TEXT value into the real command, not consumed as a help
    // request (which would have printed clap's usage/help text and exited
    // 0 with nothing attempted).
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    let sent = run_kt_agent(
        &["agent", "send", "ghost", "--help"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !sent.success,
        "must NOT silently exit 0 via help interception; stdout={}",
        sent.stdout
    );
    assert!(
        !sent.stdout.contains("Usage:") && !sent.stderr.contains("Usage:"),
        "must not print clap's help/usage text; stdout={} stderr={}",
        sent.stdout,
        sent.stderr
    );
    assert!(
        sent.stderr.contains("ghost"),
        "must surface the ordinary NotFound diagnostic naming the instance \
         (proving --help was treated as literal text, not a help request); stderr={}",
        sent.stderr
    );

    // The hyphen-value parse problem (`kt agent send x "-5 degrees"` used to
    // be a clap parse error) is likewise fixed: a leading-hyphen text value
    // reaches the ordinary send logic instead of a clap "unexpected
    // argument" failure.
    let sent2 = run_kt_agent(
        &["agent", "send", "ghost", "-5 degrees"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        sent2.stderr.contains("ghost"),
        "a hyphen-leading text value must reach the ordinary NotFound diagnostic, \
         not a clap parse error; stderr={}",
        sent2.stderr
    );
    assert!(
        !sent2.stderr.contains("unexpected argument") && !sent2.stderr.contains("Usage:"),
        "must not be rejected as a clap parse error; stderr={}",
        sent2.stderr
    );
}

/// Seed a `paused` state onto an already-registered instance directly in the
/// engine's SQLite DB (mirrors `force_state_running`) — `send`'s `NotRunning`
/// check fires purely from the persisted state, before any capability read or
/// live-process need, so this lighter DB-seed harness suffices (no real
/// process required).
fn force_state_paused(state_dir: &Path, name: &str) {
    let conn = rusqlite::Connection::open(state_db(state_dir)).expect("open state db");
    let affected = conn
        .execute(
            "UPDATE agent_instances SET state = 'paused' WHERE name = ?1",
            [name],
        )
        .expect("update state");
    assert_eq!(affected, 1, "expected to update exactly one row");
}

#[test]
fn send_on_a_paused_instance_is_not_running_with_resume_remediation_not_start() {
    // M2 fix: `NotRunning`'s remediation must match the instance's ACTUAL
    // state. Before the fix, the message UNCONDITIONALLY said "start it
    // first with: kt agent start <name>" even for a `paused` instance —
    // but `start` on a paused instance hits `InvalidTransition` (a SECOND,
    // confusing error), since the correct remediation is `kt agent resume`.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "pz", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    force_state_paused(state_dir, "pz");

    let sent = run_kt_agent(
        &["agent", "send", "pz", "hello"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !sent.success,
        "send on a paused instance must exit non-zero"
    );
    assert!(
        sent.stderr.contains("not running"),
        "stderr should name the not-running state; stderr={}",
        sent.stderr
    );
    assert!(
        sent.stderr.contains("current state: paused"),
        "stderr should name the ACTUAL current state (paused); stderr={}",
        sent.stderr
    );
    assert!(
        sent.stderr.contains("kt agent resume pz"),
        "the remediation must point at resume for a paused instance; stderr={}",
        sent.stderr
    );
    assert!(
        !sent.stderr.contains("kt agent start pz"),
        "must NOT suggest start (which would hit a second, confusing \
         InvalidTransition error on a paused instance); stderr={}",
        sent.stderr
    );
}

// ---- Story 1-6: restart count / failed cause / policy surface (AC9) + restart ----

/// Seed a `failed` instance with an `agent_runtime` record carrying a Restart
/// Policy, a restart count, and a last-known (failed) cause — directly in the
/// engine's SQLite DB. The record's pid is 0 (a policy/status seed, NOT a live
/// process), so the engine's orphan adoption on open skips it and the row stays
/// `failed`. This is how AC9's CLI surface is exercised without a real crash.
fn seed_failed_with_record(state_dir: &Path, name: &str, policy: &str, count: u32, cause: &str) {
    let conn = rusqlite::Connection::open(state_db(state_dir)).expect("open state db");
    let affected = conn
        .execute(
            "UPDATE agent_instances SET state = 'failed' WHERE name = ?1",
            [name],
        )
        .expect("update state to failed");
    assert_eq!(affected, 1, "expected to update exactly one row");
    let id: i64 = conn
        .query_row(
            "SELECT id FROM agent_instances WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .expect("instance id");
    conn.execute(
        "INSERT INTO agent_runtime \
         (instance_id, pid, start_time, restart_policy, restart_count, last_known_cause) \
         VALUES (?1, 0, 0, ?2, ?3, ?4)",
        rusqlite::params![id, policy, count as i64, cause],
    )
    .expect("insert agent_runtime record");
}

#[test]
fn list_surfaces_the_restart_count_column() {
    // AC9: `kt agent list` surfaces the per-instance restart count.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "svc", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    seed_failed_with_record(state_dir, "svc", "on-failure", 3, "crashed with code 1");

    let list = run_kt_agent(&["agent", "list"], &ctx.project_dir, state_dir);
    assert!(list.success, "list should exit 0; stderr={}", list.stderr);
    // The Restarts column header + the seeded count 3 are rendered (stdout).
    assert!(
        list.stdout.contains("Restarts"),
        "list must have a Restarts column; stdout={}",
        list.stdout
    );
    assert!(
        list.stdout.contains('3'),
        "list must show the restart count; stdout={}",
        list.stdout
    );
    // The failed state is shown too.
    assert!(list.stdout.contains("failed"), "stdout={}", list.stdout);
}

#[test]
fn show_surfaces_restart_count_policy_and_failed_cause() {
    // AC9: `kt agent show` on a failed instance surfaces the restart count, the
    // active Restart Policy, and the failed cause (result → stdout).
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "svc", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    seed_failed_with_record(
        state_dir,
        "svc",
        "on-failure",
        5,
        "crash-loop: 5 consecutive failures reached",
    );

    let show = run_kt_agent(&["agent", "show", "svc"], &ctx.project_dir, state_dir);
    assert!(show.success, "show should exit 0; stderr={}", show.stderr);
    // Runtime status block: state + policy + count.
    assert!(
        show.stdout.contains("Runtime status"),
        "show must render runtime status; stdout={}",
        show.stdout
    );
    assert!(
        show.stdout.contains("on-failure"),
        "policy; stdout={}",
        show.stdout
    );
    assert!(
        show.stdout.contains('5'),
        "restart count; stdout={}",
        show.stdout
    );
    // The failed cause (crash-loop reason) is surfaced.
    assert!(
        show.stdout.contains("crash-loop"),
        "show must surface the failed cause; stdout={}",
        show.stdout
    );
}

#[test]
fn show_surfaces_a_launch_error_failed_cause() {
    // F-Med-3 (AC9): a LAUNCH-ERROR `failed` instance has no write-ahead spawn
    // record (the `starting→failed` launch error returns before the record is
    // written), yet `kt agent show` must still surface the failed cause — via the
    // engine's event-log fallback. Register a manifest whose exec does not exist,
    // start it (fails to launch), then `show` must print the launch diagnostic.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let m = ctx.project_dir.join("bad-adapter");
    std::fs::create_dir_all(&m).unwrap();
    let body = r#"
contract_version = "1.0.0"

[adapter]
kind = "bad"

[lifecycle.start]
exec = "ktesio-no-such-binary-cli-med3"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#;
    std::fs::write(m.join("adapter.toml"), body).unwrap();
    run_kt_agent(
        &[
            "agent",
            "register",
            "bad",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    // Start fails to launch (exit non-zero) and lands the instance `failed`.
    let start = run_kt_agent(&["agent", "start", "bad"], &ctx.project_dir, state_dir);
    assert!(!start.success, "start of a bad exec should exit non-zero");

    let show = run_kt_agent(&["agent", "show", "bad"], &ctx.project_dir, state_dir);
    assert!(show.success, "show should exit 0; stderr={}", show.stderr);
    // The runtime status shows `failed`, and the failed cause (the preserved
    // launch diagnostic naming the missing exec) is surfaced on stdout.
    assert!(show.stdout.contains("failed"), "stdout={}", show.stdout);
    assert!(
        show.stdout.contains("ktesio-no-such-binary-cli-med3"),
        "show must surface the launch-error failed cause (AC9); stdout={}",
        show.stdout
    );
}

#[test]
fn start_restarts_a_failed_instance() {
    // AC3 at the CLI: `kt agent start` restarts a `failed` instance (the 1-6
    // transition row `failed → starting` permits it). Seed a `failed` instance
    // backed by a real `fake_agent` manifest, then start it → running.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    // Register a manifest instance whose exec is the real fake_agent (lingers).
    let manifest_dir = TestContext::new();
    let bin = ktesio_conformance::fake_agent_bin();
    let body = format!(
        r#"
contract_version = "1.0.0"

[adapter]
kind = "svc"

[lifecycle.start]
exec = {exec:?}
args = ["--linger-ms", "600000"]

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#,
        exec = bin.to_string_lossy(),
    );
    std::fs::write(manifest_dir.project_dir.join("adapter.toml"), body).unwrap();
    let manifest_path = manifest_dir.project_dir.to_string_lossy().to_string();
    run_kt_agent(
        &["agent", "register", "svc", "--manifest", &manifest_path],
        &ctx.project_dir,
        state_dir,
    );
    // Force it to `failed` (no record needed for the transition; policy defaults).
    let conn = rusqlite::Connection::open(state_db(state_dir)).unwrap();
    conn.execute(
        "UPDATE agent_instances SET state = 'failed' WHERE name = 'svc'",
        [],
    )
    .unwrap();
    drop(conn);

    // `kt agent start` on a failed instance restarts it → running (exit 0).
    let start = run_kt_agent(&["agent", "start", "svc"], &ctx.project_dir, state_dir);
    assert!(
        start.success,
        "start on a failed instance should restart it (exit 0); stderr={}",
        start.stderr
    );
    assert!(start.stdout.contains("running"), "stdout={}", start.stdout);

    // Teardown: stop it so the process does not linger.
    run_kt_agent(&["agent", "stop", "svc"], &ctx.project_dir, state_dir);
}

#[test]
fn list_json_emits_a_parseable_document_with_budget_seed_and_real_usage() {
    // Story 1-7 (AC5/AC9) + story 3-1 (AC-C/AC11): `kt agent list --json` writes ONE
    // parseable JSON document to stdout (and NOTHING non-JSON there), carrying a
    // top-level schema_version + per-instance objects whose `budget` is the honest
    // JSON `null` seed (budgets are 3-2) while `usage` is a REAL token-totals object
    // (zeros for a never-metered instance, not null) and `metering_source` is
    // surfaced. The metering note is on stderr.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "alpha", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );

    let run = run_kt_agent(&["agent", "list", "--json"], &ctx.project_dir, state_dir);
    assert!(
        run.success,
        "list --json should exit 0; stderr={}",
        run.stderr
    );

    // stdout is PURE JSON and re-parses (nothing else on stdout, AC9/AD-12).
    let doc: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout not JSON: {e}\n{}", run.stdout));
    // Story 3-5 bumped the Fleet document version 1 → 2 (additive: `totals` gained).
    assert_eq!(doc["schema_version"], serde_json::json!(2), "{doc}");
    let instances = doc["instances"].as_array().expect("instances array");
    assert_eq!(instances.len(), 1);
    // Story 3-5: a top-level `totals` object aggregates the rows. A never-metered
    // Fleet → zero tokens, dollars honestly absent (never a fabricated $0), not partial.
    assert!(doc["totals"].is_object(), "totals present: {doc}");
    assert_eq!(doc["totals"]["total_input_tokens"], serde_json::json!(0));
    assert_eq!(doc["totals"]["total_output_tokens"], serde_json::json!(0));
    assert!(
        doc["totals"].get("total_dollars").is_none(),
        "no Rate anywhere ⇒ no dollar total: {doc}"
    );
    assert_eq!(doc["totals"]["dollars_partial"], serde_json::json!(false));
    let entry = &instances[0];
    assert_eq!(entry["name"], serde_json::json!("alpha"));
    assert_eq!(entry["kind"], serde_json::json!("mock"));
    assert_eq!(entry["state"], serde_json::json!("registered"));
    assert_eq!(entry["restart_count"], serde_json::json!(0));
    // budget: the honest JSON null seed, NOT 0, NOT a fabricated number.
    assert_eq!(entry["budget"], serde_json::Value::Null, "{entry}");
    assert_ne!(entry["budget"], serde_json::json!(0));
    // usage: a real object with zero token totals (story 3-1) — NOT null.
    assert!(
        entry["usage"].is_object(),
        "usage must be an object: {entry}"
    );
    assert_eq!(
        entry["usage"]["cumulative_input_tokens"],
        serde_json::json!(0)
    );
    assert_eq!(
        entry["usage"]["cumulative_output_tokens"],
        serde_json::json!(0)
    );
    // The active Metering Source is surfaced (AC-C).
    assert_eq!(entry["metering_source"], serde_json::json!("self-reported"));
    // No dollar figure anywhere (tokens only — AD-8).
    assert!(entry["usage"].get("cost").is_none(), "{entry}");
    assert!(entry.get("agent_home").is_some());

    // The metering note rides on stderr (never stdout), so stdout stays valid JSON.
    assert!(
        run.stderr.contains("Usage Ledger"),
        "the metering note must be on stderr; stderr={}",
        run.stderr
    );
    assert!(
        !run.stdout.contains("Usage Ledger"),
        "stdout must be pure JSON (no note); stdout={}",
        run.stdout
    );
}

#[test]
fn list_json_on_empty_fleet_is_a_valid_empty_document() {
    // AC9: an empty Fleet with --json is still valid JSON — an empty `instances`
    // array — and the "no instances" guidance goes to STDERR so stdout parses.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    let run = run_kt_agent(&["agent", "list", "--json"], &ctx.project_dir, state_dir);
    assert!(
        run.success,
        "empty list --json should exit 0; stderr={}",
        run.stderr
    );
    let doc: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout not JSON: {e}\n{}", run.stdout));
    assert_eq!(doc["instances"], serde_json::json!([]), "{doc}");
    assert_eq!(doc["schema_version"], serde_json::json!(2));
    // Story 3-5: an empty Fleet still carries a valid all-zero / absent-dollars totals.
    assert_eq!(doc["totals"]["total_input_tokens"], serde_json::json!(0));
    assert!(doc["totals"].get("total_dollars").is_none(), "{doc}");
    // The "no instances" guidance is on stderr (so stdout is pure JSON).
    assert!(
        run.stderr.contains("No Agent Instances"),
        "empty --json guidance must be on stderr; stderr={}",
        run.stderr
    );
}

#[test]
fn human_list_shows_the_budget_column_and_real_usage_columns() {
    // Story 1-7 (AC4) + story 3-1/3-2 (AC-C/AC9): the human `list` renders a Budget
    // column — the honest `—` for an UN-budgeted instance — AND a real Usage
    // column; the metering note is on stderr. AI-45 (story 11-4): the Usage header
    // carries the "est. $" estimate qualifier like Budget's (the cells are narrow
    // and truncatable, so the qualifier lives in the headers).
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "alpha", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );

    let run = run_kt_agent(&["agent", "list"], &ctx.project_dir, state_dir);
    assert!(run.success, "list should exit 0; stderr={}", run.stderr);
    // The columns are present (headers): a Budget column (story 3-3 renamed it from
    // "Budget (tokens)" to "Budget (tok, est. $)") + Usage (real). The header may
    // truncate on a narrow terminal, so match a stable prefix.
    assert!(run.stdout.contains("Budget"), "stdout={}", run.stdout);
    assert!(run.stdout.contains("Usage"), "stdout={}", run.stdout);
    // An un-budgeted instance's budget cell renders the honest `—` absence.
    assert!(
        run.stdout.contains('—'),
        "the un-budgeted cell must render the em dash; stdout={}",
        run.stdout
    );
    // The metering note is on stderr (AD-12), never stdout.
    assert!(run.stderr.contains("Usage Ledger"), "stderr={}", run.stderr);

    // AI-45 (story 11-4): at a width where the headers render in full, the Usage
    // header carries the SAME estimate qualifier ("est. $") the Budget header
    // carries — both money-bearing columns label their dollars in the header, so
    // truncation can never strip the label off a cell.
    let wide = run_kt_agent_with_env(
        &["agent", "list"],
        &ctx.project_dir,
        state_dir,
        &[("COLUMNS", "140")],
    );
    assert!(wide.success, "list should exit 0; stderr={}", wide.stderr);
    assert!(
        wide.stdout.contains("Usage (tok, est. $)"),
        "the Usage header must carry the estimate qualifier; stdout=\n{}",
        wide.stdout
    );
    assert!(
        wide.stdout.contains("Budget (tok, est. $)"),
        "the Budget header must carry the estimate qualifier; stdout=\n{}",
        wide.stdout
    );
}

#[test]
fn show_json_surfaces_the_same_entry_shape_with_budget_seed_and_real_usage() {
    // Story 1-7 (AC5) + story 3-1: `kt agent show <name> --json` writes ONE JSON
    // document to stdout: { schema_version, instance: <the same FleetEntry shape> },
    // `budget` the honest null seed and `usage` a real token-totals object. The
    // metering note is on stderr.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "alpha", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );

    let run = run_kt_agent(
        &["agent", "show", "alpha", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        run.success,
        "show --json should exit 0; stderr={}",
        run.stderr
    );
    let doc: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout not JSON: {e}\n{}", run.stdout));
    // The show document shares the bumped Fleet schema version (2) but — the recorded
    // asymmetry — does NOT gain a Fleet `totals` (a single instance has no Fleet total;
    // its own `usage` IS its total).
    assert_eq!(doc["schema_version"], serde_json::json!(2), "{doc}");
    assert!(
        doc.get("totals").is_none(),
        "show --json must NOT carry a Fleet totals object: {doc}"
    );
    let entry = &doc["instance"];
    assert_eq!(entry["name"], serde_json::json!("alpha"));
    assert_eq!(entry["budget"], serde_json::Value::Null, "{entry}");
    assert!(
        entry["usage"].is_object(),
        "usage must be an object: {entry}"
    );
    assert_eq!(
        entry["usage"]["cumulative_input_tokens"],
        serde_json::json!(0)
    );
    assert_eq!(entry["metering_source"], serde_json::json!("self-reported"));
    // stdout is pure JSON; the note is on stderr.
    assert!(
        !run.stdout.contains("Usage Ledger"),
        "stdout={}",
        run.stdout
    );
    assert!(run.stderr.contains("Usage Ledger"), "stderr={}", run.stderr);
}

#[test]
fn human_show_surfaces_the_budget_row_and_real_usage_rows() {
    // Story 1-7 (AC4) + story 3-1/3-2 (AC-C/AC9): human `show` renders a Budget
    // (tokens) row (the honest `—` for an un-budgeted instance) plus REAL Usage
    // (tokens) + Metering source rows in the runtime-status block, with the
    // metering note on stderr.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "alpha", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );

    let run = run_kt_agent(&["agent", "show", "alpha"], &ctx.project_dir, state_dir);
    assert!(run.success, "show should exit 0; stderr={}", run.stderr);
    assert!(run.stdout.contains("Budget"), "stdout={}", run.stdout);
    assert!(run.stdout.contains("Usage"), "stdout={}", run.stdout);
    // The Metering source row + the self-reported value are surfaced (AC-C).
    assert!(
        run.stdout.contains("Metering source"),
        "stdout={}",
        run.stdout
    );
    assert!(
        run.stdout.contains("self-reported"),
        "stdout={}",
        run.stdout
    );
    // The un-budgeted budget row renders the honest `—` absence.
    assert!(run.stdout.contains('—'), "stdout={}", run.stdout);
    assert!(run.stderr.contains("Usage Ledger"), "stderr={}", run.stderr);
}

// ---- Story 6-3: the UJ-1 governance journey through documented kt commands ----

#[test]
fn uj1_governance_journey_through_documented_cli_commands() {
    // Story 6-3 (UJ-1's "govern and interact for real" contract) at the CLI:
    // every command below is a DOCUMENTED surface (`docs/commands.md`), run
    // against the REAL hermes kind — register-only, NO spawn (hermes is never
    // started here; the spawn-side governance proof is the engine e2e's
    // phases H–L). What this pins at the CLI:
    //   1. `register --kind hermes` → the kind + its `self-reported` metering
    //      source round-trip;
    //   2. `config set` accepts the UJ-1 budget keys (token ceiling + breach
    //      action, Rate + dollar Cost Cap) — validated at write time;
    //   3. `show --json` carries the budget (token ceiling, cap in integer
    //      micros, labeled), the zero usage object, and the honest
    //      `metering_source: self-reported`;
    //   4. `usage <name> --json` carries the labeled dollar fields (a Rate
    //      exists ⇒ dollars appear, `$0` labeled `estimated` at zero usage);
    //   5. the human `usage` surface names the Metering source and, on the
    //      no-Rate twin, renders the honest inert-cost cell — dollars only
    //      with a Rate (AC-B).
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    // (1) Register the hermes instance.
    let reg = run_kt_agent(
        &["agent", "register", "gw", "--kind", "hermes"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(reg.success, "register --kind hermes: stderr={}", reg.stderr);

    // (2) Configure the UJ-1 guardrails with documented keys (tokens + dollars).
    for (key, value) in [
        ("budget.tokens.cumulative", "90"),
        ("budget.breach_action", "pause"),
        ("cost.rate.input", "1.00"),
        ("cost.rate.output", "1.00"),
        ("budget.dollars.cumulative", "0.00009"),
    ] {
        let set = run_kt_agent(
            &["agent", "config", "set", "gw", key, value],
            &ctx.project_dir,
            state_dir,
        );
        assert!(
            set.success,
            "config set {key}={value}: stderr={}",
            set.stderr
        );
    }

    // (3) `show --json`: the governed shape — budget populated, usage real
    // (zero tokens), metering_source honest.
    let show = run_kt_agent(
        &["agent", "show", "gw", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(show.success, "show --json: stderr={}", show.stderr);
    let doc: serde_json::Value = serde_json::from_str(&show.stdout)
        .unwrap_or_else(|e| panic!("show --json not JSON: {e}\n{}", show.stdout));
    assert_eq!(doc["schema_version"], serde_json::json!(2), "{doc}");
    let entry = &doc["instance"];
    assert_eq!(entry["kind"], serde_json::json!("hermes"), "{entry}");
    assert_eq!(
        entry["metering_source"],
        serde_json::json!("self-reported"),
        "{entry}"
    );
    let budget = &entry["budget"];
    assert_eq!(
        budget["cumulative_limit"],
        serde_json::json!(90),
        "{budget}"
    );
    assert_eq!(
        budget["cumulative_remaining"],
        serde_json::json!(90),
        "never metered ⇒ remaining equals the ceiling"
    );
    assert_eq!(budget["breach_action"], serde_json::json!("pause"));
    // The dollar Cost Cap surfaces as integer micros ($0.00009 = 90 micros).
    assert_eq!(
        budget["cumulative_cost_cap"],
        serde_json::json!(90),
        "the cap must be integer micros: {budget}"
    );
    // Zero usage, but a Rate exists ⇒ a labeled $0 (never absent).
    let usage = &entry["usage"];
    assert_eq!(usage["cumulative_input_tokens"], serde_json::json!(0));
    assert_eq!(usage["cumulative_dollars"], serde_json::json!(0), "{usage}");
    assert_eq!(usage["estimate_label"], serde_json::json!("estimated"));

    // (4) `usage <name> --json`: the focused surface over the SAME data.
    let uj = run_kt_agent(
        &["agent", "usage", "gw", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(uj.success, "usage --json: stderr={}", uj.stderr);
    let udoc: serde_json::Value = serde_json::from_str(&uj.stdout)
        .unwrap_or_else(|e| panic!("usage --json not JSON: {e}\n{}", uj.stdout));
    assert_eq!(udoc["schema_version"], serde_json::json!(2), "{udoc}");
    assert_eq!(udoc["instance"], serde_json::json!("gw"), "{udoc}");
    assert_eq!(
        udoc["usage"]["cumulative_dollars"],
        serde_json::json!(0),
        "{udoc}"
    );
    assert_eq!(
        udoc["usage"]["estimate_label"],
        serde_json::json!("estimated")
    );
    // The metering note rides on stderr (stdout stays a pure document).
    assert!(uj.stderr.contains("Usage Ledger"), "stderr={}", uj.stderr);

    // (5) The no-Rate twin: dollars only with a Rate. A second hermes
    // instance with NO cost.rate.* renders the honest inert cost cell —
    // `—` (no fabricated $0.00) — while the metering source stays honest.
    let reg2 = run_kt_agent(
        &["agent", "register", "free", "--kind", "hermes"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(reg2.success, "register free: stderr={}", reg2.stderr);
    let ufree = run_kt_agent(&["agent", "usage", "free"], &ctx.project_dir, state_dir);
    assert!(ufree.success, "usage free: stderr={}", ufree.stderr);
    assert!(
        ufree.stdout.contains("Metering source"),
        "stdout={}",
        ufree.stdout
    );
    assert!(
        ufree.stdout.contains("self-reported"),
        "stdout={}",
        ufree.stdout
    );
    assert!(
        ufree
            .stdout
            .contains("no Rate configured — dollar features inert"),
        "the no-Rate cost cell must be the honest inert marker: stdout={}",
        ufree.stdout
    );
    assert!(
        !ufree.stdout.contains('$'),
        "no dollar figure anywhere without a Rate: stdout={}",
        ufree.stdout
    );
}

// ---- Story 7-1: the UJ-3 journey through documented kt commands ----

/// The re-exec entry for the UJ-3 journey's breach leg (mirrors
/// `agent_cli_start_helper_subprocess` above). It runs ONLY when BOTH env
/// vars are set — `KTESIO_CLI_UJ3_BREACH_HELPER` (the instance name) AND the
/// `KTESIO_CLI_UJ3_REEXEC_ARMED` arm flag the PARENT sets immediately before
/// the re-exec — so the helper can never open an engine over an arbitrary
/// state dir when its env var is set out of harness context. When armed, it
/// opens an engine over `KTESIO_STATE_DIR`, STARTS the named instance, waits —
/// in THIS engine session, where the background reaper lives — for the
/// committed `paused` state the breach produces (via the SHARED
/// `ktesio_conformance::uj3` poller), reads the LIVE Fleet row and asserts the
/// shared paused-entry shape (the same assertion the host test feeds from its
/// own facade read — the paused-entry leg is proven on BOTH paths), and only
/// then exits WITHOUT dropping the engine (crash semantics): the suspended
/// `fake_agent` survives, re-parents to init, and stays adoptable by the next
/// documented `kt agent stop`.
#[test]
fn agent_cli_uj3_breach_helper_subprocess() {
    let Ok(name) = std::env::var("KTESIO_CLI_UJ3_BREACH_HELPER") else {
        return;
    };
    // The arming guard: an env var alone is not consent — the parent must
    // have armed this re-exec deliberately.
    if std::env::var("KTESIO_CLI_UJ3_REEXEC_ARMED").as_deref() != Ok("1") {
        return;
    }
    let state = std::path::PathBuf::from(std::env::var("KTESIO_STATE_DIR").unwrap());
    let engine = Engine::open(Some(state.clone())).expect("uj3 breach helper engine open");
    engine
        .blocking()
        .start(&name)
        .expect("uj3 breach helper start");
    uj3::wait_for_state(
        &state,
        &name,
        LifecycleState::Paused,
        uj3::STATE_POLL_BUDGET,
    );
    // The paused-entry shape, from the CLI side: the engine session that owns
    // the Run reads its own LIVE Fleet row (so current-Run == cumulative
    // holds) and feeds the SAME shared assertion the host test uses.
    let entry = engine
        .blocking()
        .fleet()
        .expect("uj3 breach helper fleet read")
        .into_iter()
        .find(|e| e.name.as_str() == name)
        .expect("the flow's instance is in the Fleet");
    uj3::assert_paused_entry(&entry);
    // Exit WITHOUT dropping `engine` (no handle Drop → no kill): the breach-
    // paused process survives exactly as after an engine crash (story 1-6).
    std::process::exit(0);
}

/// Re-exec this test binary into [`agent_cli_uj3_breach_helper_subprocess`]
/// with the uj3 helper env armed — BOTH the name env and the explicit arm
/// flag (the helper refuses to run un-armed) — the file's established
/// cross-lifetime harness, specialized to the breach leg.
fn start_via_uj3_breach_helper(state_dir: &Path, name: &str) {
    let exe = std::env::current_exe().expect("test exe");
    let status = std::process::Command::new(exe)
        .args([
            "--exact",
            "agent_cli_uj3_breach_helper_subprocess",
            "--nocapture",
        ])
        .env("KTESIO_CLI_UJ3_BREACH_HELPER", name)
        .env("KTESIO_CLI_UJ3_REEXEC_ARMED", "1")
        .env("KTESIO_STATE_DIR", state_dir)
        .status()
        .expect("run uj3 breach helper subprocess");
    assert!(
        status.success(),
        "uj3 breach helper subprocess failed: {status}"
    );
}

/// Best-effort orphan guard for the journey's breach leg: between the helper's
/// crash-style exit and the stop leg, ANY failure would leak the SIGSTOP'd
/// `fake_agent` (a 10-minute linger) as a real orphan. Drop runs the
/// documented `kt agent stop <name> --timeout 0` unless disarmed — best-effort,
/// never panicking in Drop (a guard stop on an already-dead instance is a
/// harmless error, and a Drop panic would mask the test's own failure). The
/// guard lives in the PARENT test process, so it fires even if the helper
/// subprocess itself is SIGKILLed: the parent's asserts fail, the unwind runs
/// this Drop.
struct StopOrphanOnDrop<'a> {
    name: &'a str,
    working_dir: &'a Path,
    state_dir: &'a Path,
    /// The `--timeout` argument, built from the SHARED `uj3::STOP_WINDOW` (a
    /// suspended process cannot act on a graceful signal).
    timeout_arg: String,
    armed: bool,
}

impl StopOrphanOnDrop<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StopOrphanOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = run_kt_agent(
                &["agent", "stop", self.name, "--timeout", &self.timeout_arg],
                self.working_dir,
                self.state_dir,
            );
        }
    }
}

#[test]
fn uj3_governance_journey_through_documented_cli_commands_unix() {
    // Story 7-1 (FR-31), the behavioral-identity half: the SAME UJ-3 flow the
    // library host test (`crates/ktesio-engine/tests/uj3_library_host.rs`)
    // drives through the engine's `Blocking` facade is driven here through
    // DOCUMENTED `kt` commands only — register --manifest, config set,
    // config get, show --json / usage --json, stop — and EVERY assertion comes
    // from the SHARED `ktesio_conformance::uj3` module (the expectations are
    // pinned once; neither suite re-states them). The two suites drive
    // DIFFERENT roots (this one: the `KTESIO_STATE_DIR`-pinned kt harness home;
    // the host: its own temp engine root); the shared module asserts on
    // OBSERVED reads, never on paths.
    //
    // Reachability inventory (§4.x, mirroring the host test's module docs):
    // §4.1 (register --manifest / show / usage reads), §4.2 (start, the
    // breach-pause, stop), §4.3 (config set + config get provenance), and
    // §4.5 (budget/rate/cap + the recorded breaches) are exercised by this
    // flow; §4.4 (memory) and §4.6 (interaction) are facade-reachability-
    // proven by the engine's hermes e2e (tests/hermes.rs phases) and cited
    // there, not re-tested; §4.7 is the manifest contract itself; §4.9 and
    // §4.10 are out of scope (epic 8/9).
    //
    // SINGLE-LIFETIME CLI BOUNDARY (the documented `kt agent start` contract):
    // a standalone `kt agent start` kills the process on the command's clean
    // engine drop, and ANY intervening `kt` command that adopts a live process
    // kills it on ITS clean drop too — so a live agent cannot be read across
    // separate CLI invocations by construction. The flow therefore reads the
    // instance at the safe points — before the start (no process), from the
    // helper's own LIVE engine session (the shared paused-entry assertion),
    // and after the terminal stop (record cleared) — and drives the breach leg
    // through the file's established surviving-engine harness (the helper
    // above), whose engine session runs the reaper that ingests the fixture's
    // usage and commits the breach pause. `agent stop` then ADOPTS the live
    // paused process (story 1-6 AI-7) and lands the terminal state cleanly. A
    // `StopOrphanOnDrop` guard covers the window where that stop hasn't run
    // yet, so a mid-flow failure cannot leak the suspended agent.
    //
    // WINDOWS POSTURE (the `_unix` convention, and why not `#[ignore]`):
    // Windows cannot simulate cross-lifetime survival at all
    // (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE kills the child when the helper
    // exits), so the breach leg is structurally impossible there and this
    // test runtime-returns. A conditional-ignore attribute would be the
    // louder signal, but OS-conditional code is FORBIDDEN in this file by
    // the CI gate (only the engine backends and engine tests are
    // allowlisted — the gate matches the attribute text itself, even in a
    // comment) — so
    // the file's documented convention stands: the data-driven runtime return
    // plus the `_unix` SUFFIX that keeps the limitation visible in the test
    // list on every OS, while the OS-INDEPENDENT shared assertions (every one
    // this test feeds) are proven on all three OSes by the host test, which
    // runs the identical flow through the same shared module.
    if std::env::consts::OS == "windows" {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let name = uj3::FLOW_INSTANCE;

    // (1) §4.1/§4.7 register through the SHARED fixture manifest (contract v1,
    // per-OS declaration, self-reported metering, the `model` → env mapping).
    let manifest_dir = uj3::write_flow_manifest(&ctx.project_dir.join("uj3-flow"));
    let reg = run_kt_agent(
        &[
            "agent",
            "register",
            name,
            "--manifest",
            manifest_dir.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(reg.success, "register --manifest: stderr={}", reg.stderr);

    // (2) §4.3/§4.5 configure: the SHARED key/value pairs (the same writes the
    // host test performs through `Blocking::set_config`).
    for (key, value) in uj3::flow_config_pairs() {
        let set = run_kt_agent(
            &["agent", "config", "set", name, key, value],
            &ctx.project_dir,
            state_dir,
        );
        assert!(
            set.success,
            "config set {key}={value}: stderr={}",
            set.stderr
        );
    }

    // (3) §4.3 read back with provenance: the configured `model` leaf resolves
    // at the pinned layer. The SAME shared assertion the host test feeds from
    // `effective_config`.
    let cfg = run_kt_agent(
        &["agent", "config", "get", name, uj3::MODEL_KEY, "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(cfg.success, "config get --json: stderr={}", cfg.stderr);
    let cfg_doc: serde_json::Value = serde_json::from_str(&cfg.stdout)
        .unwrap_or_else(|e| panic!("config get --json not JSON: {e}\n{}", cfg.stdout));
    let leaf = &cfg_doc["entries"][0];
    assert_eq!(leaf["key"], serde_json::json!(uj3::MODEL_KEY), "{cfg_doc}");
    uj3::assert_model_leaf(
        leaf["value"].as_str().expect("a string value"),
        leaf["source"].as_str().expect("a string source"),
    );

    // (4) §4.1 pre-start `show --json`: the seeded budget + honest zero usage,
    // parsed into the SAME `FleetEntry` shape the host test's facade read
    // returns — one shared assertion, two surfaces.
    let entry: FleetEntry = show_entry(&ctx, state_dir, name);
    uj3::assert_pre_start_entry(&entry);

    // (5) pre-start `usage --json`: the focused surface over the SAME data.
    let view: UsageView = usage_view(&ctx, state_dir, name);
    uj3::assert_pre_start_usage(&view);

    // (6) §4.2 start + breach: the helper's engine session starts the instance,
    // waits for the COMMITTED paused state (usage event → BudgetEvaluator →
    // the breach-pause), asserts the shared paused-entry shape from its LIVE
    // Fleet row, then exits crash-style so the suspended process survives. The
    // orphan guard is armed FIRST: from here until the stop leg lands, any
    // failure (a killed helper included) must not leak the suspended agent.
    let timeout_arg = uj3::STOP_WINDOW.as_secs().to_string();
    let mut orphan_guard = StopOrphanOnDrop {
        name,
        working_dir: ctx.project_dir.as_path(),
        state_dir,
        timeout_arg,
        armed: true,
    };
    start_via_uj3_breach_helper(state_dir, name);

    // (7) §4.2 stop: the documented command ADOPTS the live paused process and
    // lands the terminal state. The SHARED zero window (a SIGSTOP'd process
    // cannot act on SIGTERM — the graceful window would always fully elapse),
    // the same value the host facade passes.
    let stop = run_kt_agent(
        &[
            "agent",
            "stop",
            name,
            "--timeout",
            &orphan_guard.timeout_arg,
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(stop.success, "stop: stderr={}", stop.stderr);
    assert!(stop.stdout.contains("stopped"), "stdout={}", stop.stdout);
    // The stop leg landed: no orphan possible anymore.
    orphan_guard.disarm();

    // (8) Post-stop reads (safe now: terminal state, spawn record cleared):
    // the frozen ledger — honestly a RANGE (the suspension races the emitter;
    // see `uj3::assert_stopped_usage`) — and every surface reports honestly:
    // the SAME shared assertions the host test feeds after ITS stop leg.
    let entry: FleetEntry = show_entry(&ctx, state_dir, name);
    uj3::assert_stopped_entry(&entry);
    let view: UsageView = usage_view(&ctx, state_dir, name);
    uj3::assert_stopped_usage(&view);

    // (9) The enforcement chain's records, from the SAME committed logs the
    // facade reads — exactly one breach per dimension (the token breach won
    // the pause) and a `→ paused` transition carrying the TOKEN
    // `BudgetExceeded` cause. The CLI has no breach-events command (and none
    // may be added here), so the shared committed-log readers are the
    // behavioral-identity path for these records.
    uj3::assert_flow_breaches(&uj3::read_breach_events(state_dir, name));
    uj3::assert_paused_transition_budget_exceeded(&uj3::read_transition_events(state_dir, name));
}

/// Run the documented `kt agent show <name> --json` and parse the instance row
/// back into the engine's `FleetEntry` — the SAME serde shape the host test's
/// facade read returns. The document's `schema_version` is asserted against
/// the engine's pinned Fleet constant (AD-14: "one schema, two consumers" is
/// PINNED here, not merely serde-tolerated).
fn show_entry(ctx: &TestContext, state_dir: &Path, name: &str) -> FleetEntry {
    let show = run_kt_agent(
        &["agent", "show", name, "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(show.success, "show --json: stderr={}", show.stderr);
    let doc: serde_json::Value = serde_json::from_str(&show.stdout)
        .unwrap_or_else(|e| panic!("show --json not JSON: {e}\n{}", show.stdout));
    assert_eq!(
        doc["schema_version"],
        serde_json::json!(ktesio_engine::FLEET_SCHEMA_VERSION),
        "the show document negotiates on the pinned Fleet schema version: {doc}"
    );
    serde_json::from_value(doc["instance"].clone())
        .unwrap_or_else(|e| panic!("show --json instance is not a FleetEntry: {e}\n{doc}"))
}

/// Run the documented `kt agent usage <name> --json` and parse the usage
/// object back into the engine's `UsageView`. The named usage document
/// carries the SAME Fleet `schema_version` as `list`/`show` (it serializes
/// the same fleet domain types) — asserted against the engine's pinned
/// constant, never a local literal.
fn usage_view(ctx: &TestContext, state_dir: &Path, name: &str) -> UsageView {
    let usage = run_kt_agent(
        &["agent", "usage", name, "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(usage.success, "usage --json: stderr={}", usage.stderr);
    let doc: serde_json::Value = serde_json::from_str(&usage.stdout)
        .unwrap_or_else(|e| panic!("usage --json not JSON: {e}\n{}", usage.stdout));
    assert_eq!(
        doc["schema_version"],
        serde_json::json!(ktesio_engine::FLEET_SCHEMA_VERSION),
        "the usage document carries the pinned Fleet schema version: {doc}"
    );
    serde_json::from_value(doc["usage"].clone())
        .unwrap_or_else(|e| panic!("usage --json usage is not a UsageView: {e}\n{doc}"))
}

// ---- Story 3-3: dollar cost + Cost Cap rendering (AC-B/AC10 — AD-8) ----

#[test]
fn config_set_rejects_a_malformed_rate_value_with_a_diagnostic() {
    // AC11: a malformed Rate dollar value is rejected at WRITE time (never silently
    // defaulted; a bad value must not crash). The write fails with a diagnostic and
    // the config is unchanged.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    let bad = run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "demo",
            "cost.rate.input",
            "three-dollars",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !bad.success,
        "a malformed Rate must be rejected; stdout={}",
        bad.stdout
    );
    assert!(
        bad.stderr.contains("cost.rate.input"),
        "the diagnostic must name the key; stderr={}",
        bad.stderr
    );
    // Sub-micro precision is likewise rejected.
    let submicro = run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "demo",
            "budget.dollars.cumulative",
            "5.0000001",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !submicro.success,
        "sub-micro precision must be rejected; stdout={}",
        submicro.stdout
    );
}

#[test]
fn rate_and_cap_render_labeled_dollars_in_list_json_and_human() {
    // AC10/AC-B: a Rate'd + capped instance surfaces the DOLLAR cost + cap in
    // `list --json` as INTEGER MICROS + the estimate label (NO `$` string on the
    // wire — AD-14), and in the human table THROUGH the single currency module
    // (a `$X.XX (estimated)` cell — AD-8). Even with zero usage, a Rate'd instance
    // shows `$0.00 (estimated)` (an honest labeled zero, distinct from the no-Rate
    // inert absence).
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "priced", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    // Both directions → a supplied Rate; a cumulative dollar cap.
    run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "priced",
            "cost.rate.input",
            "3.00",
        ],
        &ctx.project_dir,
        state_dir,
    );
    run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "priced",
            "cost.rate.output",
            "15.00",
        ],
        &ctx.project_dir,
        state_dir,
    );
    run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "priced",
            "budget.dollars.cumulative",
            "5.00",
        ],
        &ctx.project_dir,
        state_dir,
    );

    // --json: integer micros + label, NO `$` string.
    let json = run_kt_agent(&["agent", "list", "--json"], &ctx.project_dir, state_dir);
    assert!(
        json.success,
        "list --json should exit 0; stderr={}",
        json.stderr
    );
    let doc: serde_json::Value = serde_json::from_str(&json.stdout)
        .unwrap_or_else(|e| panic!("stdout not JSON: {e}\n{}", json.stdout));
    let entry = &doc["instances"][0];
    // The cumulative cost cap is $5.00 = 5_000_000 micros (an integer, not a string).
    assert_eq!(
        entry["budget"]["cumulative_cost_cap"],
        serde_json::json!(5_000_000),
        "the cap must be integer micros: {entry}"
    );
    // A Rate'd instance surfaces the derived cost ($0.00 at zero usage) + the label.
    assert_eq!(
        entry["usage"]["cumulative_dollars"],
        serde_json::json!(0),
        "{entry}"
    );
    assert_eq!(
        entry["usage"]["estimate_label"],
        serde_json::json!("estimated"),
        "{entry}"
    );
    // NO pre-formatted `$` string anywhere in the JSON document (AD-14).
    assert!(
        !json.stdout.contains('$'),
        "no `$` string on the wire; stdout={}",
        json.stdout
    );

    // Human table: the dollar cap cell rendered THROUGH the currency module shows a
    // `$` figure. (Since story 3-3's header treatment and story 11-4's Usage twin,
    // the narrow `list` cells render dollars BARE — the `(estimated)` qualifier
    // lives in the headers; the untruncated `show` surface asserts the inline
    // label — see `show_of_a_rated_instance_surfaces_a_labeled_cost_row`.)
    let human = run_kt_agent(&["agent", "list"], &ctx.project_dir, state_dir);
    assert!(human.success, "list should exit 0; stderr={}", human.stderr);
    assert!(
        human.stdout.contains('$'),
        "the human dollar cell must render a `$` figure; stdout={}",
        human.stdout
    );
}

#[test]
fn list_budget_dollar_label_lives_in_the_header_not_the_truncatable_cell() {
    // FR-23/AD-8 (primary review L1): the human `list` Budget column is NARROW and
    // truncates its cell with `…`. Rendering the estimate qualifier INLINE in the
    // cell (`… $0.50 (estimated) …`) would let truncation CHOP `(estimated)` and
    // leave a human staring at a bare, UNLABELED dollar. The fix moves the qualifier
    // OUT of the cell and into the COLUMN HEADER ("est. $"): the cell renders the
    // dollar value BARE (via render_dollars_bare), so there is no inline estimate
    // label in the cell to mangle, and the header carries the label instead.
    //
    // COLUMNS=116 is chosen so the Budget header + cell AND the Usage header all
    // render in full while the other columns (Agent Home) absorb the truncation
    // pressure — the columns under test are fully observable. (AI-45 widened the
    // Usage header to "Usage (tok, est. $)", which costs the table five columns of
    // slack; 116 restores the room 110 used to give the Budget cell.)
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "priced", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    // A supplied Rate (both directions) + a cumulative dollar cap → the Budget cell
    // renders a `$` cost-cap figure.
    for (key, value) in [
        ("cost.rate.input", "3.00"),
        ("cost.rate.output", "15.00"),
        ("budget.dollars.cumulative", "5.00"),
    ] {
        run_kt_agent(
            &["agent", "config", "set", "priced", key, value],
            &ctx.project_dir,
            state_dir,
        );
    }

    let human = run_kt_agent_with_env(
        &["agent", "list"],
        &ctx.project_dir,
        state_dir,
        &[("COLUMNS", "116")],
    );
    assert!(human.success, "list should exit 0; stderr={}", human.stderr);

    // (1) The estimate qualifier lives in the HEADER ("est. $") — the stable home of
    // the dollar label on this truncatable surface (FR-23). BOTH money-bearing
    // headers carry it now: Budget (story 3-3) and Usage (AI-45, story 11-4).
    assert!(
        human.stdout.contains("Budget (tok, est. $)")
            && human.stdout.contains("Usage (tok, est. $)"),
        "the Budget + Usage headers must carry the estimate qualifier 'est. $'; stdout=\n{}",
        human.stdout
    );
    // (2) The header is NOT the stale "Budget (tokens)" mislabel — the column now
    // also renders an ESTIMATED dollar Cost Cap, so "(tokens)" would misdescribe it.
    assert!(
        !human.stdout.contains("Budget (tokens)"),
        "the header must not be the stale 'Budget (tokens)' mislabel; stdout=\n{}",
        human.stdout
    );
    // (3) The Budget cell renders BARE dollar figures whose only trailing
    // parenthetical is the Breach Action `(pause)` — NEVER an inline `(estimated)`
    // (which truncation could mangle into `(esti…` glued to a dollar). The exact
    // `cum $5.00/$5.00 (pause)` cell is proof: the `$` cap + remaining are bare, the
    // estimate label is not in the cell at all, so no mangled label can appear here.
    assert!(
        human.stdout.contains("cum $5.00/$5.00 (pause)"),
        "the Budget cell must render bare dollars + only the breach action (no inline \
         estimate label to truncate); stdout=\n{}",
        human.stdout
    );
    // (3b) The Usage CELL is the AI-45 twin (story 11-4): the Rate'd dollar
    // renders BARE — `in 0 / out 0 · $0.00` — with NO inline `(estimated)` (a
    // regression to the inline form would truncate to a mangled `(es…` glued
    // to a dollar in this narrow column, the exact FR-23 hazard). Asserted on
    // the rendered row because the unit test pins the helper, not the call site.
    assert!(
        human.stdout.contains("in 0 / out 0 · $0.00"),
        "the Usage cell must render the bare Rate'd dollar; stdout=\n{}",
        human.stdout
    );
    {
        let usage_row_line = human
            .stdout
            .lines()
            .find(|l| l.contains("in 0 / out 0 · $0.00"))
            .expect("the usage row exists");
        assert!(
            !usage_row_line.contains("(estimated)") && !usage_row_line.contains("(es"),
            "the Usage row must carry NO inline estimate label fragment: {usage_row_line}"
        );
    }

    // (4) Belt-and-suspenders: the estimate qualifier is ALSO carried by the
    // always-present, never-truncated stderr metering note ("labeled estimates"),
    // which covers every rendered dollar on the surface regardless of terminal width.
    assert!(
        human.stderr.contains("labeled estimates"),
        "the stderr metering note must state dollar figures are labeled estimates; \
         stderr=\n{}",
        human.stderr
    );
}

#[test]
fn show_of_a_rated_instance_surfaces_a_labeled_cost_row() {
    // AC10: human `show` of a Rate'd instance renders a `Cost (estimated)` row with
    // a labeled dollar figure THROUGH the currency module.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "priced", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "priced",
            "cost.rate.input",
            "3.00",
        ],
        &ctx.project_dir,
        state_dir,
    );
    run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "priced",
            "cost.rate.output",
            "15.00",
        ],
        &ctx.project_dir,
        state_dir,
    );
    // A cumulative dollar cap too, so the Budget row renders a dollar Cost Cap pair.
    run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "priced",
            "budget.dollars.cumulative",
            "5.00",
        ],
        &ctx.project_dir,
        state_dir,
    );

    let show = run_kt_agent(&["agent", "show", "priced"], &ctx.project_dir, state_dir);
    assert!(show.success, "show should exit 0; stderr={}", show.stderr);
    assert!(
        show.stdout.contains("Cost (estimated)"),
        "show must render a Cost row; stdout={}",
        show.stdout
    );
    assert!(
        show.stdout.contains("$0.00 (estimated)"),
        "the Cost row must show a labeled dollar figure; stdout={}",
        show.stdout
    );
    // The `show` Value column is WIDE (no truncation), so its Budget row labels the
    // dollar Cost Cap INLINE (DollarLabel::Inline) — the full `remaining/cap (estimated)`
    // pair. At zero usage the remaining equals the $5.00 cap, so the pair is
    // `$5.00/$5.00 (estimated)`. This is the un-truncated companion to the `list`
    // truncation test, where the same label instead lives in the column header.
    assert!(
        show.stdout.contains("$5.00/$5.00 (estimated)"),
        "the `show` Budget row must render the inline-labeled dollar cap pair; \
         stdout={}",
        show.stdout
    );
}

#[test]
fn show_of_a_no_rate_instance_says_dollar_features_are_inert() {
    // AC-B: with NO Rate, dollar features are INERT and SAY SO — the human `show`
    // Cost row is an honest "no Rate configured — dollar features inert" note, never
    // a fabricated `$0.00`. Token features (the Budget/Usage rows) still render.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "norate", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );

    let show = run_kt_agent(&["agent", "show", "norate"], &ctx.project_dir, state_dir);
    assert!(show.success, "show should exit 0; stderr={}", show.stderr);
    assert!(
        show.stdout.contains("dollar features inert"),
        "with no Rate, the Cost row must say dollar features are inert; stdout={}",
        show.stdout
    );
    // No fabricated dollar figure for a no-Rate instance.
    assert!(
        !show.stdout.contains("$0.00"),
        "a no-Rate instance must NOT fabricate a $0.00 cost; stdout={}",
        show.stdout
    );
    // Token features still work: the Usage row is present.
    assert!(show.stdout.contains("Usage"), "stdout={}", show.stdout);
}

// ---- Story 3-5: the Fleet-wide total (footer + `list --json` totals) ----

#[test]
fn list_json_carries_a_fleet_totals_object_bumped_to_schema_2() {
    // Story 3-5 (AC-A/AC9): `kt agent list --json` carries a top-level `totals` object
    // and the Fleet document version is bumped 1 → 2 (additive). With one Rate'd
    // instance at zero usage, the token totals are 0 and the dollar total is a labeled
    // $0 (a Rate exists ⇒ nothing partial). Integer micros + label on the wire; NO `$`.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "priced", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    for (key, value) in [("cost.rate.input", "3.00"), ("cost.rate.output", "15.00")] {
        run_kt_agent(
            &["agent", "config", "set", "priced", key, value],
            &ctx.project_dir,
            state_dir,
        );
    }

    let run = run_kt_agent(&["agent", "list", "--json"], &ctx.project_dir, state_dir);
    assert!(
        run.success,
        "list --json should exit 0; stderr={}",
        run.stderr
    );
    let doc: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout not JSON: {e}\n{}", run.stdout));
    assert_eq!(doc["schema_version"], serde_json::json!(2), "{doc}");
    let totals = &doc["totals"];
    assert!(totals.is_object(), "totals present: {doc}");
    assert_eq!(totals["total_input_tokens"], serde_json::json!(0));
    assert_eq!(totals["total_output_tokens"], serde_json::json!(0));
    // A Rate exists ⇒ a labeled $0 dollar total (integer micros), not partial, not absent.
    assert_eq!(totals["total_dollars"], serde_json::json!(0), "{doc}");
    assert_eq!(totals["estimate_label"], serde_json::json!("estimated"));
    assert_eq!(totals["dollars_partial"], serde_json::json!(false));
    // NO `$` string on the wire (AD-14).
    assert!(
        !run.stdout.contains('$'),
        "no `$` on the wire; stdout={}",
        run.stdout
    );
}

#[test]
fn list_json_totals_carry_the_partial_flag_field() {
    // AC5 (the honesty crux) SHAPE via the CLI: a Fleet mixing a Rate'd instance with a
    // no-Rate instance carries the `dollars_partial` field in `list --json`. With zero
    // usage NEITHER instance is metered yet, so the total is a complete labeled $0
    // (`dollars_partial == false`) — this test therefore asserts the field is PRESENT +
    // the document parses, NOT that it is `true`.
    //
    // The metered-partial arithmetic that flips the flag to `true` (and the "N unpriced"
    // count) is proven exactly in the pure unit tests (`fleet.rs`, `agent.rs` footer
    // tests) + the engine `totals == ledger` integration test — a rename here keeps this
    // test's name honest about what it checks rather than over-promising `true`.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "priced", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    run_kt_agent(
        &["agent", "register", "free", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    for (key, value) in [("cost.rate.input", "3.00"), ("cost.rate.output", "15.00")] {
        run_kt_agent(
            &["agent", "config", "set", "priced", key, value],
            &ctx.project_dir,
            state_dir,
        );
    }

    let run = run_kt_agent(&["agent", "list", "--json"], &ctx.project_dir, state_dir);
    assert!(run.success, "stderr={}", run.stderr);
    let doc: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    // Two instances aggregated; the `priced` one has a Rate (dollar total present +
    // labeled), the `free` one does not — but with zero tokens neither is metered, so
    // the total is a complete labeled $0 (not partial). The shape is what we assert.
    assert_eq!(doc["instances"].as_array().unwrap().len(), 2);
    assert_eq!(doc["totals"]["total_dollars"], serde_json::json!(0));
    assert_eq!(
        doc["totals"]["estimate_label"],
        serde_json::json!("estimated")
    );
    assert!(doc["totals"].get("dollars_partial").is_some(), "{doc}");
}

#[test]
fn human_list_renders_the_fleet_total_footer() {
    // AC-A/AC-B: the human `kt agent list` renders a Fleet-wide total footer on stdout.
    // A Rate'd instance ⇒ the footer carries a labeled dollar total THROUGH the currency
    // module (a `$` figure + the estimate label); the footer names the token totals too.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "priced", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    for (key, value) in [("cost.rate.input", "3.00"), ("cost.rate.output", "15.00")] {
        run_kt_agent(
            &["agent", "config", "set", "priced", key, value],
            &ctx.project_dir,
            state_dir,
        );
    }

    let run = run_kt_agent(&["agent", "list"], &ctx.project_dir, state_dir);
    assert!(run.success, "list should exit 0; stderr={}", run.stderr);
    // The footer is on stdout (command output, AD-12).
    assert!(
        run.stdout.contains("Fleet total:"),
        "the human list must render a Fleet total footer; stdout=\n{}",
        run.stdout
    );
    // The dollar total rides through the currency module (a `$` figure) + is labeled.
    assert!(
        run.stdout.contains('$'),
        "labeled dollar total; stdout=\n{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("estimated"),
        "the Fleet total dollar figure must carry its estimate label; stdout=\n{}",
        run.stdout
    );
}

#[test]
fn human_list_footer_no_rate_shows_dash_not_zero_dollars() {
    // AC4/AC5: with NO instance Rate'd, the footer shows the token totals + an honest
    // `—` dollar marker, NEVER a fabricated `$0.00` (dollars are not derivable). The
    // per-instance rows stay honest too (tokens only).
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "norate", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );

    let run = run_kt_agent(&["agent", "list"], &ctx.project_dir, state_dir);
    assert!(run.success, "list should exit 0; stderr={}", run.stderr);
    assert!(
        run.stdout.contains("Fleet total:"),
        "stdout=\n{}",
        run.stdout
    );
    // No fabricated $0.00 Fleet total (no Rate ⇒ dollars honestly absent).
    assert!(
        !run.stdout.contains("$0.00"),
        "a no-Rate Fleet must not fabricate a $0.00 total; stdout=\n{}",
        run.stdout
    );
    // The footer line itself carries the honest absent-dollars marker.
    assert!(
        run.stdout.contains("dollars not derived"),
        "the footer must say dollars are not derived (no Rate); stdout=\n{}",
        run.stdout
    );
}

// ---- Story 2-1: `kt agent config set` / `get` (AC10, AC-B, AC7, AD-12) ----

#[test]
fn config_set_then_get_shows_the_value_on_stdout() {
    // AC10/AC-A end-to-end: set a known key, then `config get <name> <key>`
    // prints the effective value to stdout (bare, no quotes) and exits 0.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );

    let set = run_kt_agent(
        &["agent", "config", "set", "demo", "model", "gpt-4"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(set.success, "set should exit 0; stderr={}", set.stderr);
    assert!(set.stdout.contains("Set"), "stdout={}", set.stdout);

    // Single-key get: the bare value on stdout.
    let get = run_kt_agent(
        &["agent", "config", "get", "demo", "model"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(get.success, "get should exit 0; stderr={}", get.stderr);
    assert!(
        get.stdout.lines().any(|l| l.trim() == "gpt-4"),
        "stdout should print the bare value; stdout={}",
        get.stdout
    );

    // Whole-config get: a Key/Value table on stdout, provenance note on stderr.
    let get_all = run_kt_agent(
        &["agent", "config", "get", "demo"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        get_all.success,
        "get-all should exit 0; stderr={}",
        get_all.stderr
    );
    assert!(
        get_all.stdout.contains("model"),
        "stdout should list the key; stdout={}",
        get_all.stdout
    );
    // Story 2-3: the stale "provenance arrives in Epic 2.3" deferral note is
    // RETIRED (the Source column now IS the provenance) — it must appear NOWHERE.
    assert!(
        !get_all.stderr.contains("Epic 2.3") && !get_all.stdout.contains("Epic 2.3"),
        "the stale Epic 2.3 deferral note must be gone; stdout={} stderr={}",
        get_all.stdout,
        get_all.stderr
    );
    // The provenance now rides on STDOUT as a "Source" column (result → stdout).
    assert!(
        get_all.stdout.contains("Source") && get_all.stdout.contains("instance"),
        "config get must show a Source column with the instance layer; stdout={}",
        get_all.stdout
    );
}

#[test]
fn config_get_effective_is_empty_before_any_set() {
    // AC-C / review decision #1: engine + kind defaults ship EMPTY in 2-1, so
    // before any `set` the effective config has no keys — `config get` exits 0
    // and prints the "no config keys set" info line (never a fabricated default).
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );

    let get = run_kt_agent(
        &["agent", "config", "get", "demo"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(get.success, "get should exit 0; stderr={}", get.stderr);
    assert!(
        get.stdout.contains("no config keys set"),
        "empty effective config should say so; stdout={}",
        get.stdout
    );
    // The seeded identity key `name` is NOT surfaced (patch #4).
    assert!(
        !get.stdout.contains("name"),
        "identity key must not appear; stdout={}",
        get.stdout
    );
}

#[test]
fn config_set_instance_value_is_read_back() {
    // AC-A/AC10: a key set at the INSTANCE layer is read back by `config get`.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );

    run_kt_agent(
        &["agent", "config", "set", "demo", "model", "claude-opus"],
        &ctx.project_dir,
        state_dir,
    );
    let get = run_kt_agent(
        &["agent", "config", "get", "demo", "model"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        get.stdout.lines().any(|l| l.trim() == "claude-opus"),
        "instance value should read back; stdout={}",
        get.stdout
    );
}

#[test]
fn config_set_child_under_scalar_fails_closed() {
    // Review patch #3 end-to-end: nesting a child under an existing scalar is
    // rejected (non-zero exit) naming the conflict; nothing persisted.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    run_kt_agent(
        &["agent", "config", "set", "demo", "agent.a", "v1"],
        &ctx.project_dir,
        state_dir,
    );
    let config_path = state_dir.join("agents").join("demo").join("config.toml");
    let before = std::fs::read(&config_path).unwrap();

    let run = run_kt_agent(
        &["agent", "config", "set", "demo", "agent.a.b", "v2"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !run.success,
        "shape conflict must exit non-zero; stdout={}",
        run.stdout
    );
    assert!(
        run.stderr.contains("agent.a"),
        "stderr should name the conflicting ancestor; stderr={}",
        run.stderr
    );
    assert_eq!(
        std::fs::read(&config_path).unwrap(),
        before,
        "failed write must leave config byte-unchanged"
    );
}

#[test]
fn config_set_unknown_key_is_rejected_with_suggestion_and_config_unchanged() {
    // AC-B: an unknown key outside `agent.*` is rejected (non-zero exit), the
    // suggestion is on stderr, and the on-disk config.toml is byte-unchanged.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );

    let config_path = state_dir.join("agents").join("demo").join("config.toml");
    let before = std::fs::read(&config_path).expect("read config before");

    // `modle` is a near-miss for the known key `model`.
    let run = run_kt_agent(
        &["agent", "config", "set", "demo", "modle", "gpt-4"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !run.success,
        "unknown key must exit non-zero; stdout={}",
        run.stdout
    );
    assert!(
        run.stderr.contains("modle"),
        "stderr should name the offending key; stderr={}",
        run.stderr
    );
    assert!(
        run.stderr.contains("model"),
        "stderr should suggest the nearest key; stderr={}",
        run.stderr
    );

    // AC-B atomicity: nothing persisted — config byte-unchanged.
    let after = std::fs::read(&config_path).expect("read config after");
    assert_eq!(before, after, "rejected write must not touch config.toml");
}

#[test]
fn config_set_agent_pass_through_key_round_trips_verbatim() {
    // AC7: an `agent.*` key writes successfully and round-trips verbatim.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );

    let set = run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "demo",
            "agent.custom_flag",
            "verbatim-value",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        set.success,
        "agent.* set should exit 0; stderr={}",
        set.stderr
    );

    let get = run_kt_agent(
        &["agent", "config", "get", "demo", "agent.custom_flag"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        get.stdout.lines().any(|l| l.trim() == "verbatim-value"),
        "pass-through key must round-trip verbatim; stdout={}",
        get.stdout
    );
}

#[test]
fn config_set_accepts_leading_dash_values_without_the_dashdash_separator() {
    // Story 11-2 (AI-26): a leading-hyphen VALUE used to die at the clap parse
    // layer (exit 2, "unexpected argument"). With `allow_hyphen_values` on the
    // `value` positional (mirroring `send`'s `text`), `config set svc key -x`
    // round-trips the value VERBATIM — and the explicit `--` separator form
    // still works for callers who prefer it.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );

    // Bare leading-dash value: no `--` needed.
    let set = run_kt_agent(
        &["agent", "config", "set", "demo", "agent.dash_flag", "-x"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        set.success,
        "a leading-dash value must exit 0, not a clap parse error; stdout={} stderr={}",
        set.stdout, set.stderr
    );
    assert!(
        !set.stderr.contains("unexpected argument"),
        "must not be rejected as a clap parse error; stderr={}",
        set.stderr
    );
    let get = run_kt_agent(
        &["agent", "config", "get", "demo", "agent.dash_flag"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        get.stdout.lines().any(|l| l.trim() == "-x"),
        "the leading-dash value must round-trip verbatim; stdout={}",
        get.stdout
    );

    // A double-dash VALUE also parses literally now.
    let set2 = run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "demo",
            "agent.dash_flag2",
            "--model-x",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        set2.success,
        "a `--`-prefixed value must exit 0; stdout={} stderr={}",
        set2.stdout, set2.stderr
    );
    let get2 = run_kt_agent(
        &["agent", "config", "get", "demo", "agent.dash_flag2"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        get2.stdout.lines().any(|l| l.trim() == "--model-x"),
        "the `--`-prefixed value must round-trip verbatim; stdout={}",
        get2.stdout
    );

    // The classic `--` separator form still works.
    let set3 = run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "demo",
            "agent.dash_flag3",
            "--",
            "-y",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        set3.success,
        "the `--` separator form must still work; stdout={} stderr={}",
        set3.stdout, set3.stderr
    );
    let get3 = run_kt_agent(
        &["agent", "config", "get", "demo", "agent.dash_flag3"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        get3.stdout.lines().any(|l| l.trim() == "-y"),
        "the `--`-separated value must round-trip verbatim; stdout={}",
        get3.stdout
    );
}

#[test]
fn config_set_secret_on_a_flag_targeted_key_warns_on_stderr_but_exits_zero() {
    // Story 11-2 (AI-33): a `secret:NAME` value set on a FLAG-targeted key
    // SUCCEEDS (warn-only — never a rejection) AND prints a steering warning to
    // STDERR naming the key, the argv leak risk, and the env/file alternative
    // (the stdout success line is unchanged). Needs a manifest adapter whose
    // `[config.model]` maps onto a flag — the native `mock` maps env (quiet).
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let manifest_body = format!("{VALID_MANIFEST}\n[config.model]\nflag = \"--model\"\n");
    let m = manifest_dir(&ctx.project_dir, &manifest_body);
    let reg = run_kt_agent(
        &[
            "agent",
            "register",
            "flg",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        reg.success,
        "manifest register should exit 0; stderr={}",
        reg.stderr
    );

    let set = run_kt_agent(
        &["agent", "config", "set", "flg", "model", "secret:MY_SECRET"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        set.success,
        "the secret→flag set must still SUCCEED (warn-only); stdout={} stderr={}",
        set.stdout, set.stderr
    );
    assert!(
        set.stderr.contains("model"),
        "the stderr warning must name the key; stderr={}",
        set.stderr
    );
    assert!(
        set.stderr.contains("FLAG"),
        "the stderr warning must name the flag-target leak risk; stderr={}",
        set.stderr
    );
    assert!(
        set.stderr.contains("env") && set.stderr.contains("file"),
        "the stderr warning must name the env/file alternative; stderr={}",
        set.stderr
    );
    // Result → stdout, warning → stderr (AD-12): the success line is untouched.
    assert!(
        set.stdout.contains("Set"),
        "stdout keeps the success confirmation; stdout={}",
        set.stdout
    );

    // The quiet control: the same secret on an ENV-targeted (mock's `model`)
    // instance warns NOTHING.
    run_kt_agent(
        &["agent", "register", "envq", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    let quiet = run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "envq",
            "model",
            "secret:MY_SECRET",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        quiet.success,
        "the env-target set must exit 0; stderr={}",
        quiet.stderr
    );
    assert!(
        !quiet.stderr.contains("flag"),
        "an env-targeted secret must not warn; stderr={}",
        quiet.stderr
    );
}

#[test]
fn config_get_unknown_instance_exits_nonzero() {
    // A `get` on an unregistered instance is the uniform not-found diagnostic on
    // stderr with a non-zero exit.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    let run = run_kt_agent(
        &["agent", "config", "get", "ghost"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !run.success,
        "get on a ghost must exit non-zero; stdout={}",
        run.stdout
    );
    assert!(
        run.stderr.contains("ghost"),
        "stderr should name the missing instance; stderr={}",
        run.stderr
    );
}

// ---- Story 2-2: the `agent.*`-unvalidated marker in `config get` (AC-B/AC7) ----

#[test]
fn config_get_table_prefix_exits_nonzero_and_names_the_child_leaves() {
    // AI-25 (story 11-4): `budget` has no effective VALUE of its own but IS a
    // prefix of effective leaves once one is set. The honest rejection stays
    // (stderr + non-zero exit, stdout clean) but now NAMES the child leaves so the
    // operator is steered to the right `get` in one step. The leaf path (a real
    // value prints) and the plain unknown-key path (no children named) are pinned
    // unchanged by the companion tests + the second half of this one.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "demo",
            "budget.tokens.cumulative",
            "500000",
        ],
        &ctx.project_dir,
        state_dir,
    );

    // The table prefix: non-zero exit, nothing on stdout, children named on stderr.
    let run = run_kt_agent(
        &["agent", "config", "get", "demo", "budget"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !run.success,
        "a table prefix must exit non-zero; stdout={}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("500000"),
        "stdout must stay clean of a partial document; stdout={}",
        run.stdout
    );
    assert!(
        run.stderr.contains("budget.tokens.cumulative"),
        "stderr must name the effective child leaf; stderr={}",
        run.stderr
    );
    assert!(
        run.stderr.contains("table prefix"),
        "stderr must say WHY (a config table prefix, not a plain unknown key); stderr={}",
        run.stderr
    );

    // A key that is NEITHER a value NOR a prefix keeps the plain unknown-key
    // diagnostic — no children named, because none exist.
    let unknown = run_kt_agent(
        &["agent", "config", "get", "demo", "cost"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !unknown.success,
        "an unknown key must exit non-zero; stdout={}",
        unknown.stdout
    );
    assert!(
        unknown
            .stderr
            .contains("has no effective value for config key 'cost'"),
        "the plain unknown-key diagnostic; stderr={}",
        unknown.stderr
    );
    assert!(
        !unknown.stderr.contains("table prefix"),
        "a non-prefix key must NOT read as a table; stderr={}",
        unknown.stderr
    );

    // The leaf path is unchanged: the exact key with a value still prints it.
    let leaf = run_kt_agent(
        &["agent", "config", "get", "demo", "budget.tokens.cumulative"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        leaf.success,
        "a leaf get must exit 0; stderr={}",
        leaf.stderr
    );
    assert!(
        leaf.stdout.lines().any(|l| l.trim() == "500000"),
        "leaf value on stdout; stdout={}",
        leaf.stdout
    );
}

#[test]
fn config_get_marks_agent_pass_through_leaf_unvalidated_and_known_key_validated() {
    // AC-B / AC7 at the CLI: `config get <name>` renders a "Validated" marker per
    // row — an `agent.*` leaf is marked `unvalidated` (it bypassed known-key
    // validation), while a KNOWN key (`model`) is marked `validated`. The marker
    // rides on STDOUT (part of the result table, AD-12). Story 2-3 ADDS the
    // "Source" column beside it and RETIRES the stale Epic 2.3 note.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    // A known key (validated) and an agent.* pass-through key (unvalidated).
    run_kt_agent(
        &["agent", "config", "set", "demo", "model", "gpt-4"],
        &ctx.project_dir,
        state_dir,
    );
    run_kt_agent(
        &["agent", "config", "set", "demo", "agent.custom_flag", "on"],
        &ctx.project_dir,
        state_dir,
    );

    let get = run_kt_agent(
        &["agent", "config", "get", "demo"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(get.success, "get should exit 0; stderr={}", get.stderr);
    // The Validated column header is present, and the unvalidated marker appears
    // for the agent.* leaf (result → stdout).
    assert!(
        get.stdout.contains("Validated"),
        "config get must have a Validated column; stdout={}",
        get.stdout
    );
    assert!(
        get.stdout.contains("unvalidated"),
        "the agent.* leaf must be marked unvalidated; stdout={}",
        get.stdout
    );
    // Both keys are listed.
    assert!(get.stdout.contains("model"), "stdout={}", get.stdout);
    assert!(
        get.stdout.contains("agent.custom_flag"),
        "stdout={}",
        get.stdout
    );
    // Story 2-3: the "Source" column now sits beside "Validated" on STDOUT, and
    // the stale Epic 2.3 deferral note is gone from both streams.
    assert!(
        get.stdout.contains("Source"),
        "config get must show a Source column beside Validated; stdout={}",
        get.stdout
    );
    assert!(
        !get.stderr.contains("Epic 2.3") && !get.stdout.contains("Epic 2.3"),
        "the stale Epic 2.3 deferral note must be gone; stdout={} stderr={}",
        get.stdout,
        get.stderr
    );
}

#[test]
fn config_get_known_key_only_shows_no_unvalidated_marker() {
    // AC-B companion: with ONLY a known key set (no agent.* leaf), `config get`
    // shows the value marked `validated` and NO `unvalidated` marker anywhere —
    // the marker is present only for pass-through leaves.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    run_kt_agent(
        &["agent", "config", "set", "demo", "model", "gpt-4"],
        &ctx.project_dir,
        state_dir,
    );

    let get = run_kt_agent(
        &["agent", "config", "get", "demo"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(get.success, "get should exit 0; stderr={}", get.stderr);
    // No `unvalidated` marker anywhere (the only key is a known one).
    assert!(
        !get.stdout.contains("unvalidated"),
        "a known-key-only config must show NO unvalidated marker; stdout={}",
        get.stdout
    );
    // The affirmative `validated` marker IS present — checked unambiguously (not
    // via a bare `contains("validated")`, which is a substring of "unvalidated").
    // Since no `unvalidated` occurs (asserted above), stripping it is a no-op here;
    // the check stays correct even if the two ever co-occur in some future row.
    assert!(
        get.stdout.replace("unvalidated", "").contains("validated"),
        "the known key must be marked validated (standalone token); stdout={}",
        get.stdout
    );
}

// ---- Story 2-3: per-value SOURCE-LAYER rendering (human + --json) (AC-A/AC3/AC4) ----

#[test]
fn config_get_human_shows_source_column_with_the_winning_layer() {
    // AC3 (the FR-13 heart): `config get <name>` gains a "Source" column naming
    // each leaf's winning layer. A key set at the instance layer shows source
    // `instance`; the column rides on STDOUT (result), and the stale Epic 2.3
    // deferral note is gone.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    run_kt_agent(
        &["agent", "config", "set", "demo", "model", "gpt-4"],
        &ctx.project_dir,
        state_dir,
    );

    let get = run_kt_agent(
        &["agent", "config", "get", "demo"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(get.success, "get should exit 0; stderr={}", get.stderr);
    // The Source column header + the `instance` label for the set key (STDOUT).
    assert!(
        get.stdout.contains("Source"),
        "config get must have a Source column; stdout={}",
        get.stdout
    );
    assert!(
        get.stdout.contains("instance"),
        "the instance-layer key must show source `instance`; stdout={}",
        get.stdout
    );
    // The stale deferral note is retired from both streams.
    assert!(
        !get.stdout.contains("Epic 2.3") && !get.stderr.contains("Epic 2.3"),
        "the stale Epic 2.3 note must be gone; stdout={} stderr={}",
        get.stdout,
        get.stderr
    );
}

#[test]
fn config_get_json_emits_source_per_leaf_on_stdout() {
    // AC4 + AD-12: `config get <name> --json` writes ONE parseable JSON document
    // to stdout — a versioned doc whose per-leaf objects carry { key, value,
    // source, unvalidated }. A known key set at the instance layer is
    // instance-sourced + validated; an agent.* leaf is unvalidated.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    run_kt_agent(
        &["agent", "config", "set", "demo", "model", "gpt-4"],
        &ctx.project_dir,
        state_dir,
    );
    run_kt_agent(
        &["agent", "config", "set", "demo", "agent.custom_flag", "on"],
        &ctx.project_dir,
        state_dir,
    );

    let get = run_kt_agent(
        &["agent", "config", "get", "demo", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        get.success,
        "config get --json should exit 0; stderr={}",
        get.stderr
    );
    let doc: serde_json::Value = serde_json::from_str(&get.stdout)
        .unwrap_or_else(|e| panic!("stdout must be pure JSON: {e}; stdout={}", get.stdout));
    assert_eq!(doc["schema_version"], serde_json::json!(1), "{doc}");
    let entries = doc["entries"].as_array().unwrap();

    let model = entries.iter().find(|e| e["key"] == "model").unwrap();
    assert_eq!(model["value"], serde_json::json!("gpt-4"));
    assert_eq!(model["source"], serde_json::json!("instance"));
    assert_eq!(model["unvalidated"], serde_json::json!(false));

    let flag = entries
        .iter()
        .find(|e| e["key"] == "agent.custom_flag")
        .unwrap();
    assert_eq!(flag["source"], serde_json::json!("instance"));
    assert_eq!(flag["unvalidated"], serde_json::json!(true));
    // Nothing but JSON on stdout (no note leaked there).
    assert!(
        !get.stdout.contains("Epic 2.3"),
        "stdout must stay pure JSON; stdout={}",
        get.stdout
    );
}

#[test]
fn config_get_single_key_json_emits_just_that_leaf() {
    // AC4: the single-key `config get <name> <key> --json` form emits exactly one
    // leaf with its value + source, matching the human single-key value.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    run_kt_agent(
        &["agent", "config", "set", "demo", "model", "claude-opus"],
        &ctx.project_dir,
        state_dir,
    );

    let get = run_kt_agent(
        &["agent", "config", "get", "demo", "model", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(get.success, "get should exit 0; stderr={}", get.stderr);
    let doc: serde_json::Value = serde_json::from_str(&get.stdout)
        .unwrap_or_else(|e| panic!("stdout must be pure JSON: {e}; stdout={}", get.stdout));
    let entries = doc["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "single-key --json emits one leaf; {doc}");
    assert_eq!(entries[0]["key"], serde_json::json!("model"));
    assert_eq!(entries[0]["value"], serde_json::json!("claude-opus"));
    assert_eq!(entries[0]["source"], serde_json::json!("instance"));
}

#[test]
fn config_get_persists_effective_config_snapshot_at_start() {
    // AC5 end-to-end through the CLI: starting an instance writes the
    // effective-config snapshot (effective-config.json) into the Agent Home,
    // carrying model=<v> tagged `instance`. Uses a live `fake_agent` manifest (the
    // builtin `mock` is inert — its start rejects before the snapshot write), and
    // reads the Agent Home path from `register`'s stdout (path authority — kt
    // never constructs it). The started process is cleaned up on this CLI exit
    // (kill-on-drop), so no separate stop is needed.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let m = fake_agent_manifest(&ctx.project_dir, &["--linger-ms", "600000"]);

    let reg = run_kt_agent(
        &[
            "agent",
            "register",
            "snapcli",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(reg.success, "register should exit 0; stderr={}", reg.stderr);
    // The registered Agent Home path is the stdout result line (an absolute path).
    let home = reg
        .stdout
        .lines()
        .find(|l| l.contains("agents") && l.contains("snapcli"))
        .unwrap_or_else(|| {
            panic!(
                "register stdout should name the home; stdout={}",
                reg.stdout
            )
        })
        .trim()
        .to_string();

    run_kt_agent(
        &["agent", "config", "set", "snapcli", "model", "gpt-4o"],
        &ctx.project_dir,
        state_dir,
    );
    let start = run_kt_agent(&["agent", "start", "snapcli"], &ctx.project_dir, state_dir);
    assert!(
        start.success,
        "start should exit 0; stderr={}",
        start.stderr
    );

    // The snapshot exists in the Agent Home and carries the resolved value +
    // provenance (written before the `starting` transition).
    let snapshot_path = std::path::Path::new(&home).join("effective-config.json");
    assert!(
        snapshot_path.is_file(),
        "the effective-config snapshot must exist at {snapshot_path:?}"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&snapshot_path).unwrap()).unwrap();
    assert_eq!(doc["schema_version"], serde_json::json!(1), "{doc}");
    let model = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "model")
        .unwrap();
    assert_eq!(model["value"], serde_json::json!("gpt-4o"));
    assert_eq!(model["source"], serde_json::json!("instance"));
}

// ============================================================================
// Story 2-4: SECRETS — the no-leak guarantee (FR-14, NFR-6, AD-10).
//
// The sentinel cleartext used across the matrix. If it ever appears in a
// no-leak surface (engine/instance log, event payload, config get --json, the
// snapshot), the guarantee is broken. It DOES appear in the adapter's native
// mechanism (the --dump file) and under `config get --reveal`.
// ============================================================================

const SECRET_SENTINEL: &str = "s3cr3t-sentinel-VALUE-xyz";

/// A `kt` run with ONE extra environment variable set on the child (so the env
/// SecretResolver can resolve `secret:NAME` at start). Mirrors `run_kt_agent` but
/// adds `env(key, value)`; the child `kt` process inherits it, and the engine it
/// spawns reads it via `std::env::var`. Local to the secret tests (the shared
/// helper does not need a general env knob).
fn run_kt_agent_env(
    args: &[&str],
    working_dir: &Path,
    state_dir: &Path,
    env_key: &str,
    env_val: &str,
) -> (bool, String, String) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_kt"))
        .args(args)
        .current_dir(working_dir)
        .env("KTESIO_NO_UPDATE_CHECK", "1")
        .env("KTESIO_STATE_DIR", state_dir)
        .env(env_key, env_val)
        .output()
        .expect("Failed to execute kt");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// Write a `fake_agent` manifest that (a) dumps its received argv + env to
/// `dump_path` at startup (`--dump`, the config-mapping observation point),
/// (b) drops a readiness `--marker` file at startup (the AI-35/38 readiness
/// handshake — a test waits for it before polling the dump), and (c) maps the
/// unified `model` key into the native env var `MODEL`
/// (`[config.model] env = "MODEL"`, the SHARED builder's config-env chain —
/// story 10-1). So a `model = "secret:NAME"` leaf, once resolved, lands in the
/// child's `MODEL` env — captured in the dump as `env=MODEL=<cleartext>`.
fn fake_agent_manifest_secret_env(
    dir: &Path,
    dump_path: &Path,
    marker_path: &Path,
) -> std::path::PathBuf {
    let m = dir.join("fake-agent-secret-adapter");
    ManifestFixture::fake_agent(
        "fake",
        &[
            "--linger-ms",
            "600000",
            "--dump",
            &*dump_path.to_string_lossy(),
            "--marker",
            &*marker_path.to_string_lossy(),
        ],
    )
    .config_env("model", "MODEL")
    .write(&m)
}

/// Recursively collect the text of every file under `dir` into one string (for the
/// no-leak sweep of an Agent Home / logs directory). Missing/binary files are
/// skipped. Also returns each scanned path so a failure names WHERE a leak was.
fn read_tree_text(dir: &Path) -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(read_tree_text(&path));
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            out.push((path, text));
        }
    }
    out
}

#[test]
fn secret_reaches_the_adapter_but_never_leaks_and_reveal_shows_it() {
    // THE no-leak matrix (AC-B, the security heart of FR-14/NFR-6), end-to-end
    // through the real `kt` binary:
    //   - a `secret:MODEL_KEY` leaf resolves (env resolver) to a sentinel cleartext;
    //   - the sentinel REACHES the adapter's native mechanism (env=MODEL=<sentinel>
    //     in the --dump file) — the value is USABLE (AC9 delivery);
    //   - the sentinel appears in NONE of: the persisted effective-config snapshot,
    //     `config get --json`, the human `config get`, or ANY file in the Agent Home
    //     (logs + event payloads included) — the MASK appears instead (AC-A/AC-B);
    //   - `config get --json --reveal` DOES carry the sentinel (AC-C, the sole
    //     un-mask), and the default `--json` carries the mask.
    //
    // Cross-OS (AI-35/AI-38, story 11-5): this test used to be runtime-gated
    // Linux-only because its POSITIVE-delivery half observed the one-shot
    // `kt agent start`'s agent — a process that macOS CI never let become
    // observable and that Windows' kill-on-close reaps the instant `kt`
    // exits. Both legs now run it, via two commissioned fixes: the start
    // goes through the `start_via_surviving_engine` harness (the agent is
    // provably launched by the helper's engine before the helper exits — on
    // Windows the job-close kill that follows still leaves the startup
    // artifacts on disk), and the observation is a READINESS HANDSHAKE, not
    // a wall-clock race: the manifest passes `--marker`, the test waits for
    // the marker file (written by fake_agent at startup, before its `--dump`)
    // and THEN for the dump — proceeding as soon as the agent is provably up,
    // with generous bounded deadlines, on every OS. The no-leak/masking logic
    // under test is OS-agnostic engine code (`ResolvedValue::display()`,
    // snapshot + JSON serialization). The resolver env var is set on THIS
    // process (unique name) so the re-exec'd helper's engine inherits it.
    let env_key = "KTESIO_CLI_SECRET_E2E_KEY";
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    // The dump file the agent writes its received env into (outside the state dir,
    // so the no-leak Agent-Home sweep does not scan the intended-cleartext dump),
    // plus the readiness marker the handshake waits on.
    let dump = ctx.project_dir.join("agent-received.dump");
    let marker = ctx.project_dir.join("agent-ready.marker");
    let m = fake_agent_manifest_secret_env(&ctx.project_dir, &dump, &marker);

    // Register + set `model = secret:NAME` (the reference is what is stored).
    let reg = run_kt_agent(
        &[
            "agent",
            "register",
            "sek",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(reg.success, "register failed; stderr={}", reg.stderr);
    let home = reg
        .stdout
        .lines()
        .find(|l| l.contains("agents") && l.contains("sek"))
        .expect("register stdout names the home")
        .trim()
        .to_string();
    let set = run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "sek",
            "model",
            &format!("secret:{env_key}"),
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(set.success, "set failed; stderr={}", set.stderr);

    // Start via the surviving (crashed-engine) helper with the resolver env var
    // set on THIS process (the helper's engine — and the agent it spawns —
    // inherits it, resolving the secret to the sentinel, which
    // apply_config_mapping delivers into env MODEL). The var is restored at the
    // end of the test (the save/restore idiom the supervisor secret test uses;
    // under nextest each test is its own process, so even a mid-test failure
    // cannot leak past it).
    let env_value = std::env::var_os(env_key);
    std::env::set_var(env_key, SECRET_SENTINEL);
    start_via_surviving_engine(state_dir, "sek");

    // (POSITIVE) The sentinel REACHED the adapter's native env (the value is usable).
    let dump_text = {
        // Readiness handshake, not a wall-clock race: wait for the agent's
        // startup marker first (it is provably up), then for the dump line.
        // Both bounds are generous for a loaded CI runner; each poll returns
        // the moment its file appears, so the happy path pays nothing.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if marker.exists() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the agent's readiness marker never appeared at {marker:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Ok(t) = std::fs::read_to_string(&dump) {
                if t.contains("env=MODEL=") {
                    break t;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the agent never wrote its dump at {dump:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    assert!(
        dump_text.contains(&format!("env=MODEL={SECRET_SENTINEL}")),
        "the resolved cleartext must reach the adapter's native env; dump=\n{dump_text}"
    );

    // (NO-LEAK) The persisted snapshot masks the secret (never the sentinel).
    let snapshot_path = std::path::Path::new(&home).join("effective-config.json");
    let snapshot = std::fs::read_to_string(&snapshot_path).unwrap();
    assert!(
        !snapshot.contains(SECRET_SENTINEL),
        "the effective-config snapshot leaked the secret:\n{snapshot}"
    );
    assert!(
        snapshot.contains("secret:****"),
        "the snapshot must carry the mask; snapshot=\n{snapshot}"
    );

    // (NO-LEAK) Every file in the Agent Home — logs + event payloads included —
    // is free of the sentinel. This is the log + event-payload half of AC-B.
    for (path, text) in read_tree_text(std::path::Path::new(&home)) {
        assert!(
            !text.contains(SECRET_SENTINEL),
            "the secret leaked into an Agent Home file: {path:?}\n{text}"
        );
    }

    // (NO-LEAK) `config get --json` (default, no --reveal) masks the secret.
    let get_json = run_kt_agent(
        &["agent", "config", "get", "sek", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        get_json.success,
        "get --json failed; stderr={}",
        get_json.stderr
    );
    assert!(
        !get_json.stdout.contains(SECRET_SENTINEL),
        "config get --json leaked the secret by default:\n{}",
        get_json.stdout
    );
    assert!(
        get_json.stdout.contains("secret:****"),
        "config get --json must mask by default:\n{}",
        get_json.stdout
    );

    // (NO-LEAK) The human `config get` table masks too (same display path).
    let get_human = run_kt_agent(
        &["agent", "config", "get", "sek"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !get_human.stdout.contains(SECRET_SENTINEL),
        "the human config get leaked the secret:\n{}",
        get_human.stdout
    );

    // (REVEAL) `config get --json --reveal` re-resolves LIVE and DOES carry the
    // sentinel — the sole un-mask (AC-C). Needs the resolver var in the env for
    // the read (the explicit single-run env helper sets it for this child).
    let (rok, rout, rerr) = run_kt_agent_env(
        &["agent", "config", "get", "sek", "--json", "--reveal"],
        &ctx.project_dir,
        state_dir,
        env_key,
        SECRET_SENTINEL,
    );
    assert!(rok, "get --reveal failed; stderr={rerr}");
    let doc: serde_json::Value = serde_json::from_str(&rout)
        .unwrap_or_else(|e| panic!("--reveal --json must be pure JSON: {e}; out={rout}"));
    let model = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "model")
        .expect("the model leaf is present");
    assert_eq!(
        model["value"],
        serde_json::json!(SECRET_SENTINEL),
        "--reveal must emit the unmasked cleartext; doc={doc}"
    );

    // Restore the resolver env var (the save/restore idiom; see the start above).
    match env_value {
        Some(v) => std::env::set_var(env_key, v),
        None => std::env::remove_var(env_key),
    }
}

#[test]
fn secret_single_key_reveal_shows_only_that_leaf_and_default_masks() {
    // AC-C single-key form: `config get <name> <key> --json [--reveal]`. Default
    // masks that one leaf; --reveal shows just its cleartext. A --reveal on a
    // NON-secret leaf is a harmless no-op (the plain value).
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let dump = ctx.project_dir.join("agent.dump");
    let m = fake_agent_manifest_secret_env(&ctx.project_dir, &dump, &dump.with_extension("marker"));

    let reg = run_kt_agent(
        &[
            "agent",
            "register",
            "one",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(reg.success, "register failed; stderr={}", reg.stderr);
    // A secret leaf (agent.token) + a plain leaf (agent.mode).
    run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "one",
            "agent.token",
            "secret:TOKEN_KEY",
        ],
        &ctx.project_dir,
        state_dir,
    );
    run_kt_agent(
        &["agent", "config", "set", "one", "agent.mode", "fast"],
        &ctx.project_dir,
        state_dir,
    );

    // Default single-key --json masks the secret leaf.
    let masked = run_kt_agent(
        &["agent", "config", "get", "one", "agent.token", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(masked.success, "get failed; stderr={}", masked.stderr);
    let d: serde_json::Value = serde_json::from_str(&masked.stdout).unwrap();
    assert_eq!(d["entries"].as_array().unwrap().len(), 1);
    assert_eq!(d["entries"][0]["value"], serde_json::json!("secret:****"));

    // --reveal on the single secret leaf shows only its cleartext.
    let (rok, rout, _e) = run_kt_agent_env(
        &[
            "agent",
            "config",
            "get",
            "one",
            "agent.token",
            "--json",
            "--reveal",
        ],
        &ctx.project_dir,
        state_dir,
        "TOKEN_KEY",
        SECRET_SENTINEL,
    );
    assert!(rok, "reveal single-key failed");
    let d: serde_json::Value = serde_json::from_str(&rout).unwrap();
    assert_eq!(d["entries"].as_array().unwrap().len(), 1);
    assert_eq!(d["entries"][0]["value"], serde_json::json!(SECRET_SENTINEL));

    // --reveal on a NON-secret leaf is a harmless no-op: the plain value.
    let (pok, pout, _e) = run_kt_agent_env(
        &[
            "agent",
            "config",
            "get",
            "one",
            "agent.mode",
            "--json",
            "--reveal",
        ],
        &ctx.project_dir,
        state_dir,
        "TOKEN_KEY",
        SECRET_SENTINEL,
    );
    assert!(pok, "reveal on non-secret failed");
    let d: serde_json::Value = serde_json::from_str(&pout).unwrap();
    assert_eq!(d["entries"][0]["value"], serde_json::json!("fast"));
}

#[test]
fn unresolved_secret_rejects_the_start_with_a_diagnostic_and_no_state_change() {
    // AC5/AC9: a `secret:NAME` that resolves in NEITHER env nor the (absent)
    // secrets file REJECTS the start — non-zero exit, a stderr diagnostic naming the
    // NAME (never a value), and the instance stays in its PRIOR state (the snapshot
    // is not even written). The env var is deliberately NOT set.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let dump = ctx.project_dir.join("agent.dump");
    let m = fake_agent_manifest_secret_env(&ctx.project_dir, &dump, &dump.with_extension("marker"));

    let reg = run_kt_agent(
        &[
            "agent",
            "register",
            "noresolve",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(reg.success, "register failed; stderr={}", reg.stderr);
    let home = reg
        .stdout
        .lines()
        .find(|l| l.contains("agents") && l.contains("noresolve"))
        .expect("register stdout names the home")
        .trim()
        .to_string();
    run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "noresolve",
            "model",
            "secret:DEFINITELY_UNSET_KEY_XYZ",
        ],
        &ctx.project_dir,
        state_dir,
    );

    // Start WITHOUT the env var → unresolved → non-zero, diagnostic names the NAME.
    let start = run_kt_agent(
        &["agent", "start", "noresolve"],
        &ctx.project_dir,
        state_dir,
    );
    assert!(!start.success, "start must fail on an unresolved secret");
    assert!(
        start.stderr.contains("DEFINITELY_UNSET_KEY_XYZ"),
        "the diagnostic must name the secret NAME; stderr={}",
        start.stderr
    );
    // The instance stayed in its prior state: the snapshot was never written (the
    // resolution failure rejects before the snapshot write + the `starting`
    // transition).
    let snapshot_path = std::path::Path::new(&home).join("effective-config.json");
    assert!(
        !snapshot_path.exists(),
        "an unresolved secret must reject the start before the snapshot is written"
    );
    // And `agent list --json` still shows it as `registered` (never
    // `running`/`failed` from a half-launch). Assert on the JSON `state` field —
    // deterministic committed state, not the width-dependent human table.
    let list = run_kt_agent(&["agent", "list", "--json"], &ctx.project_dir, state_dir);
    let doc: serde_json::Value = serde_json::from_str(&list.stdout)
        .unwrap_or_else(|e| panic!("list --json not JSON: {e}\n{}", list.stdout));
    let state = doc["instances"][0]["state"].as_str().unwrap_or("");
    assert_eq!(
        state, "registered",
        "the instance must stay registered after a rejected start; list=\n{}",
        list.stdout
    );
}

// ---- Story 3-2: Token-Budget config + Fleet-detail budget surface (AC-C, AC9) ----

#[test]
fn budget_config_keys_set_and_surface_in_list_json() {
    // AC9: a budgeted instance surfaces the ceiling(s) + remaining + Breach Action
    // in `list --json` (tokens only, deterministic — no process spawn needed since
    // the budget is a config read). AC-C: the budget + action keys are settable.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    // Set a cumulative budget + a non-default action.
    let set = run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "demo",
            "budget.tokens.cumulative",
            "5000",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        set.success,
        "set budget should exit 0; stderr={}",
        set.stderr
    );
    run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "demo",
            "budget.breach_action",
            "stop",
        ],
        &ctx.project_dir,
        state_dir,
    );

    let list = run_kt_agent(&["agent", "list", "--json"], &ctx.project_dir, state_dir);
    assert!(
        list.success,
        "list --json should exit 0; stderr={}",
        list.stderr
    );
    let doc: serde_json::Value = serde_json::from_str(&list.stdout)
        .unwrap_or_else(|e| panic!("list --json not JSON: {e}\n{}", list.stdout));
    let budget = &doc["instances"][0]["budget"];
    assert_eq!(
        budget["cumulative_limit"], 5000,
        "the cumulative ceiling surfaces in --json; doc={}",
        list.stdout
    );
    // Never metered → remaining equals the ceiling (ledger is zero).
    assert_eq!(budget["cumulative_remaining"], 5000);
    assert_eq!(budget["breach_action"], "stop");
    // Tokens only — no dollar cap/headroom.
    assert!(budget.get("cost_cap").is_none(), "no dollars: {budget}");

    // The human table shows the token budget cell (not `—`). The cell may truncate
    // on a narrow terminal, so match a stable prefix; the exact values are asserted
    // on --json above. `show` uses a wider Value column, so assert the full cell
    // there for the action + remaining.
    let show = run_kt_agent(&["agent", "show", "demo"], &ctx.project_dir, state_dir);
    assert!(
        show.stdout.contains("cum 5000/5000") && show.stdout.contains("stop"),
        "human show must render the full token budget cell; stdout=\n{}",
        show.stdout
    );
}

#[test]
fn an_unbudgeted_instance_shows_the_honest_absent_budget() {
    // AC9: an instance with NO budget configured shows an honest absent budget
    // (`null` in --json, `—` in the human table) — never a fabricated ceiling.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "bare", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );

    let list = run_kt_agent(&["agent", "list", "--json"], &ctx.project_dir, state_dir);
    let doc: serde_json::Value = serde_json::from_str(&list.stdout)
        .unwrap_or_else(|e| panic!("list --json not JSON: {e}\n{}", list.stdout));
    assert_eq!(
        doc["instances"][0]["budget"],
        serde_json::Value::Null,
        "an un-budgeted instance's budget is null; doc={}",
        list.stdout
    );

    // The human show shows the `—` absence in the budget cell. (Story 3-3 renamed
    // the `show` row "Budget (tokens)" → "Budget" since it now covers tokens AND the
    // dollar Cost Cap.)
    let human = run_kt_agent(&["agent", "show", "bare"], &ctx.project_dir, state_dir);
    assert!(
        human.stdout.contains("Budget") && human.stdout.contains('—'),
        "human show must render the honest absent budget; stdout=\n{}",
        human.stdout
    );
}

#[test]
fn a_malformed_budget_value_is_rejected_at_write_time() {
    // AC-C: a malformed budget number / unknown Breach-Action string is rejected at
    // config-write time (non-zero exit, a clear diagnostic on stderr, nothing
    // persisted) — never silently defaulted.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "demo", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );

    let bad_num = run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "demo",
            "budget.tokens.per_run",
            "lots",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(!bad_num.success, "a non-numeric budget must be rejected");
    assert!(
        bad_num.stderr.contains("budget.tokens.per_run"),
        "the diagnostic names the key; stderr={}",
        bad_num.stderr
    );

    let bad_action = run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "demo",
            "budget.breach_action",
            "throttle",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        !bad_action.success,
        "an unknown breach action must be rejected"
    );
    assert!(
        bad_action.stderr.contains("pause") && bad_action.stderr.contains("throttle"),
        "the diagnostic names the accepted set + the offending value; stderr={}",
        bad_action.stderr
    );

    // Nothing was persisted: the budget is still absent in --json.
    let list = run_kt_agent(&["agent", "list", "--json"], &ctx.project_dir, state_dir);
    let doc: serde_json::Value = serde_json::from_str(&list.stdout).unwrap();
    assert_eq!(
        doc["instances"][0]["budget"],
        serde_json::Value::Null,
        "a rejected write must persist nothing; doc={}",
        list.stdout
    );
}

// ---- Story 4-2: `kt agent logs <name> [--follow]` (AC-A, AC-B, AC-H) ----

#[test]
fn logs_reads_retained_output_after_the_instance_stops_unix() {
    // AC-A: register, start via the surviving-engine harness (so real
    // content genuinely accrues before that starting session exits), stop
    // it via a SEPARATE `kt agent stop` invocation (which must first adopt
    // the still-live orphan), then a THIRD, separate `kt agent logs`
    // invocation reads the complete retained history — the common case,
    // and (unlike 4.1's `send`) expected to succeed even across a clean
    // process boundary, since a stopped instance's log file needs no live
    // handle at all.
    if ktesio_engine::OsId::current() == ktesio_engine::OsId::Windows {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let m = fake_agent_manifest_with_interaction(
        &ctx.project_dir,
        &["--heartbeat-ms", "40", "--linger-ms", "600000"],
        "guaranteed",
    );
    run_kt_agent(
        &[
            "agent",
            "register",
            "svc",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    start_via_surviving_engine(state_dir, "svc");

    let stopped = run_kt_agent(&["agent", "stop", "svc"], &ctx.project_dir, state_dir);
    assert!(
        stopped.success,
        "stop of the adopted instance should exit 0; stderr={}",
        stopped.stderr
    );

    let logs = run_kt_agent(&["agent", "logs", "svc"], &ctx.project_dir, state_dir);
    assert!(logs.success, "logs should exit 0; stderr={}", logs.stderr);
    assert!(
        logs.stdout.contains("[agent-out]"),
        "attributed agent-out lines must be present; stdout={}",
        logs.stdout
    );
    assert!(
        logs.stdout.contains("heartbeat"),
        "the retained heartbeat output must be readable; stdout={}",
        logs.stdout
    );
    assert!(
        logs.stdout.contains("[engine]"),
        "an engine-attributed transition line must be present; stdout={}",
        logs.stdout
    );
}

#[test]
fn logs_follow_on_an_adopted_instance_reads_history_and_exits_cleanly_on_stop_unix() {
    // AC-H at the CLI layer — the mirror image of 4.1's AC-D CLI test
    // (`send_on_an_adopted_instance_exits_nonzero_with_interaction_unavailable_unix`):
    // THERE, `send` on this SAME kind of instance exits non-zero
    // (`InteractionUnavailable`); HERE, `kt agent logs --follow` reads the
    // pre-crash captured history with NO error and exits CLEANLY once the
    // instance transitions out of running — proving AC-H (no live
    // handle/daemon needed to read/follow an adopted instance) together
    // with AC-C (a clean, non-hanging exit) through the real `kt` binary.
    //
    // RENAMED (fix pass, L3, review of #80) from
    // `logs_follow_on_an_adopted_running_instance_streams_new_output`: the
    // OLD name claimed exactly what this test's own doc comment already
    // admitted it doesn't prove — mirrors how the engine-level sibling test
    // is correctly named
    // `adopted_instance_can_be_followed_from_a_fresh_engine_session`
    // (`ktesio-engine/tests/logs.rs`), not "...streams_new_output". Verified
    // EMPIRICALLY (both here and at the engine level, unaffected by this
    // fix pass's crash-safety redesign — an adopted handle still gets no
    // capture pipeline of its own, by design, matching pre-fix behavior
    // exactly) that this is NOT achievable: `start_via_surviving_engine`'s
    // starting session EXITS (the crash simulation) before this test's
    // `--follow` ever runs, and a process exit terminates EVERY thread in
    // it, including the output-capture threads — there is no "the old
    // capture keeps running in the background" outcome, matching the
    // story's OWN Dev Notes qualifier ("...capture threads running... in
    // WHICHEVER engine session... has NOT YET EXITED"). So no further bytes
    // ever reach either capture file once the starting session is gone —
    // `fake_agent` itself survives (re-parented to init), but its
    // stdout/stderr redirects have no active writer-side participant left
    // to extend them further (the RAW files themselves would still accept
    // direct writes from the still-alive agent process, crash-immune as
    // ever — it is specifically the ATTRIBUTED capture's tailer, which only
    // the now-gone starting session ever ran, that stops). This test
    // instead proves the actual, honest, achievable AC-H claim: the
    // pre-crash history is followable with no error, and follow exits
    // cleanly (never hanging) once the instance is stopped by a THIRD,
    // separate command.
    //
    // Runtime-skip on Windows: the SAME cross-lifetime-survival limitation
    // `start_via_surviving_engine`'s other CLI tests document
    // (`agent_cli.rs:906-918`) — inherited, not new. NO `#[cfg]`
    // (data-driven; this file is outside the backends allowlist).
    if ktesio_engine::OsId::current() == ktesio_engine::OsId::Windows {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let m = fake_agent_manifest_with_interaction(
        &ctx.project_dir,
        &["--heartbeat-ms", "40", "--linger-ms", "600000"],
        "guaranteed",
    );
    run_kt_agent(
        &[
            "agent",
            "register",
            "svc",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    start_via_surviving_engine(state_dir, "svc");

    // Spawn `kt agent logs svc --follow` in the BACKGROUND (not
    // `run_kt_agent`, which blocks for full completion) so the main thread
    // can stop the instance concurrently and observe the follow process
    // exit cleanly on its own.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_kt"))
        .args(["agent", "logs", "svc", "--follow"])
        .current_dir(&ctx.project_dir)
        .env("KTESIO_NO_UPDATE_CHECK", "1")
        .env("KTESIO_STATE_DIR", state_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn kt agent logs --follow");

    // Give follow a moment to complete its initial one-shot dump and enter
    // its poll loop, then stop the instance from a SEPARATE process.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let stopped = run_kt_agent(&["agent", "stop", "svc"], &ctx.project_dir, state_dir);
    assert!(
        stopped.success,
        "stop of the adopted instance should exit 0; stderr={}",
        stopped.stderr
    );

    // The follow process must exit on its own within a bounded time —
    // never hang — once it observes the non-running state (AC-C).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "kt agent logs --follow must not hang after the instance stops"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "follow must exit 0 on a clean stop-triggered end"
    );

    let mut stdout = String::new();
    let mut stderr = String::new();
    {
        use std::io::Read;
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
    }

    assert!(
        stdout.contains("[agent-out]"),
        "the pre-crash captured history must be readable via follow's initial dump; stdout={stdout}"
    );
    assert!(stdout.contains("heartbeat"), "stdout={stdout}");
    assert!(
        stderr.contains("no further output"),
        "an honest exit note must be printed once the instance stops; stderr={stderr}"
    );
}

#[test]
fn logs_on_a_never_started_instance_is_empty_not_an_error() {
    // AC-A negative-space case: a registered-but-never-started instance
    // returns an honest empty result, not an error.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "register", "nat", "--kind", "mock"],
        &ctx.project_dir,
        state_dir,
    );
    let run = run_kt_agent(&["agent", "logs", "nat"], &ctx.project_dir, state_dir);
    assert!(
        run.success,
        "logs on a never-started instance should exit 0; stderr={}",
        run.stderr
    );
    assert!(
        run.stdout.trim().is_empty(),
        "no output should be retained yet; stdout={}",
        run.stdout
    );
}

#[test]
fn logs_on_an_unregistered_name_is_not_found() {
    // The deliberate-improvement check from Task 5's second bullet: unlike
    // `transition_events`/`budget_breach_events`'s precedent (which never
    // check the registry at all), `read_agent_log` is the first CLI-facing
    // consumer of this shape, so a truly unregistered name is a NotFound
    // diagnostic, not a silently-empty result.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let run = run_kt_agent(&["agent", "logs", "ghost"], &ctx.project_dir, state_dir);
    assert!(
        !run.success,
        "logs on an unregistered name should exit non-zero"
    );
    assert!(
        run.stderr.contains("No Agent Instance named 'ghost'"),
        "stderr should name the missing instance; stderr={}",
        run.stderr
    );
}

// ===========================================================================
// Story 4-3 — COMPATIBILITY SURFACE TESTS (FR-26, PRD §7, DC-5/DC-6)
// ===========================================================================
//
// These are THE compatibility gate for `kt`'s two machine-facing contracts:
// the `--json` wire shapes and the numeric exit codes. They run in the
// `test` CI job (`cargo nextest run --workspace --all-targets`) on all three
// OSes, so an unannounced change fails CI everywhere.
//
// WHAT "ALL THREE OSes" PRECISELY COVERS (fix pass, 2026-07-21 — the earlier
// blanket claim was overstated and an adversarial pass proved it):
//   * Every WIRE SHAPE is frozen in BOTH its unpriced and its PRICED form (see
//     `priced_instance` and the `*_PRICED_KEYS` constants — before the fix, every
//     fixture was Rate-less, so `skip_serializing_if` hid the entire dollar half
//     of `UsageView`/`FleetTotals`/`BudgetView` from every assertion). None of
//     these tests spawns a child, so they genuinely gate all three OSes.
//   * Every `schema_version` is pinned BY LITERAL VALUE, never by comparison to
//     the constant it was stamped from.
//   * Exit codes 0/1/2/3/4 are asserted end-to-end through the real binary,
//     cross-OS.
//   * Exit codes 5 and 6 are the ONE exception: every route to them needs a
//     genuinely running child, and the surviving-engine harness is Unix-only.
//     They are gated cross-OS in two composed halves instead — the mapper tests
//     in `cli::agent::tests` prove which diagnostic each condition PRODUCES and
//     how it classifies, and the 0..=4 tests here prove `main` wires `classify`
//     to the process status. On Unix, code 5 additionally has two end-to-end
//     assertions (`pause_unsupported_*_unix`, `send_unsupported_*_unix`).
//   * Any test that cannot run on Windows carries a `_unix` SUFFIX, so a skip is
//     visible in the test list rather than reported as a silent pass.
//
// WHY BESPOKE TESTS AND NOT `cargo-semver-checks` (recorded for reviewers):
// the repo's semver-check job is (a) currently DORMANT and (b) Rust-API-only
// by design — it compares the `ktesio-engine` *Rust* public API. It does NOT
// see serialized JSON (a `#[serde(rename)]` that silently renames a wire
// field PASSES semver-checks) and it does not see process exit codes at all.
// It also never inspects the `kt` binary crate, where the CLI-local documents
// (`ShowDocument`/`UsageDocument`/`FleetUsageDocument`/`ConfigDocument`) and
// the exit-code classifier live. So these assertions — not semver-checks —
// are what make an unannounced wire/exit-code change fail CI.
//
// HOW THEY FAIL LOUDLY: each `--json` document's key-set is FROZEN with a
// sorted `assert_eq!`, so ADDING, renaming, or removing a field breaks the
// assertion and forces an intentional edit (the PRD §7 "announce" gate),
// rather than silently changing the contract. `schema_version` is pinned by
// value for the same reason.

/// The FROZEN `FleetEntry` key-set — the per-instance object shared by
/// `list --json` (rows) and `show --json` (`instance`). `failed_cause` is
/// `skip_serializing_if` and absent for a healthy instance, so it is not in
/// the always-present set asserted here.
const FLEET_ENTRY_KEYS: &[&str] = &[
    "agent_home",
    "budget",
    "kind",
    "metering_source",
    "name",
    "restart_count",
    "restart_policy",
    "state",
    "usage",
];

/// The FROZEN `UsageView` key-set for an instance with NO Rate configured —
/// the four always-present token counters. The three dollar fields
/// (`cumulative_dollars`, `current_run_dollars`, `estimate_label`) are
/// `skip_serializing_if` and appear ONLY when a Rate exists (AC-B: no Rate ⇒
/// no dollar figure, never a fabricated `$0.00`).
const USAGE_VIEW_TOKEN_KEYS: &[&str] = &[
    "cumulative_input_tokens",
    "cumulative_output_tokens",
    "current_run_input_tokens",
    "current_run_output_tokens",
];

/// The FROZEN `FleetTotals` key-set with no Rate anywhere in the Fleet.
/// `total_dollars` + `estimate_label` are `skip_serializing_if` and absent
/// until at least one instance is priced.
const FLEET_TOTALS_KEYS: &[&str] = &[
    "dollars_partial",
    "total_input_tokens",
    "total_output_tokens",
    "unpriced_count",
];

// ---------------------------------------------------------------------------
// The PRICED (Rate-configured) key-sets — fix pass, H2
// ---------------------------------------------------------------------------
//
// Every constant above describes the UNPRICED shape, because every fixture
// registered a plain `mock` with no Rate — and all three dollar-bearing types
// hide their dollar fields behind `skip_serializing_if = "Option::is_none"`.
// So until this fix the ENTIRE priced half of the wire was unfrozen: an
// adversarial pass added a new `blended_rate` field to `UsageView`, populated
// only in `with_dollars()`, and all 69 compatibility tests still passed.
//
// The constants below freeze that second shape. They are reached by the
// `priced_instance` fixture, which configures a Rate (both directions are
// required for a Rate to be live) plus both token scopes and both dollar Cost
// Cap scopes, so every optional field materializes at once — making these the
// MAXIMAL key-sets of `UsageView`, `FleetTotals`, and `BudgetView`.

/// The FROZEN `UsageView` key-set once a Rate exists: the four token counters
/// PLUS the three dollar fields (integer micros + the `estimated`/`reconciled`
/// label — never a `$` string, AD-8/AD-14).
const USAGE_VIEW_PRICED_KEYS: &[&str] = &[
    "cumulative_dollars",
    "cumulative_input_tokens",
    "cumulative_output_tokens",
    "current_run_dollars",
    "current_run_input_tokens",
    "current_run_output_tokens",
    "estimate_label",
];

/// The FROZEN `FleetTotals` key-set once at least one instance is priced:
/// the unpriced set PLUS `total_dollars` + `estimate_label`.
const FLEET_TOTALS_PRICED_KEYS: &[&str] = &[
    "dollars_partial",
    "estimate_label",
    "total_dollars",
    "total_input_tokens",
    "total_output_tokens",
    "unpriced_count",
];

/// The FROZEN, MAXIMAL `BudgetView` key-set — a Rate plus BOTH token scopes and
/// BOTH dollar Cost Cap scopes configured, so no field is skipped. The dollar
/// cap fields (`per_run_cost_cap`, `per_run_dollars_remaining`,
/// `cumulative_cost_cap`, `cumulative_dollars_remaining`, `estimate_label`) sat
/// in the same unfrozen region as `UsageView`'s dollars before this fix.
const BUDGET_VIEW_PRICED_KEYS: &[&str] = &[
    "breach_action",
    "cumulative_cost_cap",
    "cumulative_dollars_remaining",
    "cumulative_limit",
    "cumulative_remaining",
    "estimate_label",
    "per_run_cost_cap",
    "per_run_dollars_remaining",
    "per_run_limit",
    "per_run_remaining",
];

// ---------------------------------------------------------------------------
// The MEMORY documents — story 6-6, THE ONE ANNOUNCED KEY-SET EDIT
// ---------------------------------------------------------------------------
//
// Epic 5 shipped `kt agent memory attach|detach` human-output-only, deferring
// the wire surface to Epic 6 with Story 5-1's DC-6 "ONE intentional announced
// key-set edit" obligation attached (epics.md:614-618). Contract v1's freeze
// lands it here, in the SAME change that tags 1.0.0, announced in the
// CHANGELOG/RELEASE_NOTES. After this edit the memory documents below are
// frozen exactly like every set above: any further change is a breaking v1
// wire change under the published policy (docs/adapter-contract.md).
//
// AI-64 maximal-fixture rule: the documents have no `skip_serializing_if`
// fields, but their VALUES vary by kind/guarantee/delivery — so the fixtures
// exercise BOTH kinds (`filesystem` + `native`), BOTH guarantee levels, and
// BOTH delivery facts (`declared` true via the mock's built-in `memory.dir`
// mapping, false via the native backing's nothing-to-declare rule), so every
// field materializes on the wire at least once.

/// The FROZEN `memory attach --json` (MemoryAttachDocument) key-set.
const MEMORY_ATTACH_KEYS: &[&str] = &[
    "declared",
    "dir",
    "guarantee",
    "instance",
    "kind",
    "schema_version",
];

/// The FROZEN `memory detach --json` (MemoryDetachDocument) key-set.
const MEMORY_DETACH_KEYS: &[&str] = &["instance", "schema_version"];

/// Sorted JSON object keys of `value`, for the frozen key-set assertions.
fn sorted_keys(value: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .unwrap_or_else(|| panic!("expected a JSON object, got: {value}"))
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// Assert `value`'s key-set is EXACTLY `expected` (a frozen wire contract).
fn assert_frozen_keys(value: &serde_json::Value, expected: &[&str], what: &str) {
    assert_eq!(
        sorted_keys(value),
        expected.iter().map(|k| k.to_string()).collect::<Vec<_>>(),
        "the frozen `{what}` key-set changed — this is a v1 compatibility \
         surface (PRD §7). If the change is intentional, announce it and \
         update this assertion deliberately. Document: {value}",
    );
}

/// Register a `mock` instance in a fresh state dir and return the contexts.
fn registered_mock(name: &str) -> (TestContext, TestContext) {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let run = run_kt_agent(
        &["agent", "register", name, "--kind", "mock"],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert!(
        run.success,
        "register should succeed; stderr={}",
        run.stderr
    );
    (ctx, state)
}

/// Register a `mock` instance and configure EVERY dollar-bearing dimension
/// (fix pass, H2), so the PRICED wire shape materializes: both Rate directions
/// (both are required for a Rate to be live), both token budget scopes, and both
/// dollar Cost Cap scopes. With this fixture `UsageView`, `FleetTotals`, and
/// `BudgetView` all serialize their maximal key-sets — the half of the wire that
/// no `registered_mock`-based fixture can ever reach.
fn priced_instance(name: &str) -> (TestContext, TestContext) {
    let (ctx, state) = registered_mock(name);
    let state_dir = state.project_dir.as_path();
    for (key, value) in [
        ("cost.rate.input", "3.00"),
        ("cost.rate.output", "15.00"),
        ("budget.tokens.per_run", "1000"),
        ("budget.tokens.cumulative", "500000"),
        ("budget.dollars.per_run", "1.50"),
        ("budget.dollars.cumulative", "25.00"),
    ] {
        let set = run_kt_agent(
            &["agent", "config", "set", name, key, value],
            &ctx.project_dir,
            state_dir,
        );
        assert!(
            set.success,
            "config set {key}={value} should succeed; stderr={}",
            set.stderr
        );
    }
    (ctx, state)
}

// ---------------------------------------------------------------------------
// 5.3 — `--json` key-set freeze + `schema_version` pins (every read command)
// ---------------------------------------------------------------------------

#[test]
fn list_json_document_key_set_and_schema_version_are_frozen() {
    // `list --json` = FleetListing { schema_version, totals, instances } with
    // FleetEntry rows and a FleetTotals aggregate. Freezes FleetListing +
    // FleetEntry + UsageView + FleetTotals in one real-wire assertion.
    let (ctx, state) = registered_mock("alpha");
    let run = run_kt_agent(
        &["agent", "list", "--json"],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);

    let doc: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout not pure JSON: {e}\n{}", run.stdout));
    assert_frozen_keys(
        &doc,
        &["instances", "schema_version", "totals"],
        "FleetListing",
    );
    assert_eq!(doc["schema_version"], serde_json::json!(2), "{doc}");
    assert_frozen_keys(&doc["totals"], FLEET_TOTALS_KEYS, "FleetTotals");

    let entry = &doc["instances"].as_array().expect("instances array")[0];
    assert_frozen_keys(entry, FLEET_ENTRY_KEYS, "FleetEntry");
    assert_frozen_keys(&entry["usage"], USAGE_VIEW_TOKEN_KEYS, "UsageView");
}

#[test]
fn show_json_document_key_set_and_schema_version_are_frozen() {
    // `show --json` = ShowDocument { schema_version, instance: FleetEntry }.
    let (ctx, state) = registered_mock("alpha");
    let run = run_kt_agent(
        &["agent", "show", "alpha", "--json"],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);

    let doc: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout not pure JSON: {e}\n{}", run.stdout));
    assert_frozen_keys(&doc, &["instance", "schema_version"], "ShowDocument");
    assert_eq!(doc["schema_version"], serde_json::json!(2), "{doc}");
    assert_frozen_keys(&doc["instance"], FLEET_ENTRY_KEYS, "FleetEntry");
    assert_frozen_keys(
        &doc["instance"]["usage"],
        USAGE_VIEW_TOKEN_KEYS,
        "UsageView",
    );
}

#[test]
fn usage_json_named_document_key_set_and_schema_version_are_frozen() {
    // Story 4-3 net-new: `usage <name> --json` = UsageDocument
    // { schema_version, instance, usage: UsageView } — a SINGLE versioned
    // document (NOT NDJSON: usage is a snapshot, not a stream), riding the
    // SHARED FLEET_SCHEMA_VERSION because it serializes fleet-domain content.
    let (ctx, state) = registered_mock("alpha");
    let run = run_kt_agent(
        &["agent", "usage", "alpha", "--json"],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);

    let doc: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout not pure JSON: {e}\n{}", run.stdout));
    assert_frozen_keys(
        &doc,
        &["instance", "schema_version", "usage"],
        "UsageDocument",
    );
    // Reuses FLEET_SCHEMA_VERSION (2) — no `usage`-specific constant was minted.
    assert_eq!(doc["schema_version"], serde_json::json!(2), "{doc}");
    assert_eq!(doc["instance"], serde_json::json!("alpha"));
    assert_frozen_keys(&doc["usage"], USAGE_VIEW_TOKEN_KEYS, "UsageView");
}

#[test]
fn usage_json_fleet_document_key_set_and_schema_version_are_frozen() {
    // Story 4-3 net-new: `usage --json` (no name) = FleetUsageDocument
    // { schema_version, totals: FleetTotals } — the Fleet-wide scope FR-22 names.
    let (ctx, state) = registered_mock("alpha");
    let run = run_kt_agent(
        &["agent", "usage", "--json"],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);

    let doc: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout not pure JSON: {e}\n{}", run.stdout));
    assert_frozen_keys(&doc, &["schema_version", "totals"], "FleetUsageDocument");
    assert_eq!(doc["schema_version"], serde_json::json!(2), "{doc}");
    assert_frozen_keys(&doc["totals"], FLEET_TOTALS_KEYS, "FleetTotals");
}

#[test]
fn usage_json_totals_equal_the_list_json_totals_exactly() {
    // FR-22 honesty: the standalone `usage` command is a different SURFACE over
    // the SAME aggregate — never a second, independently-summed number. Both
    // documents must carry byte-identical totals.
    let (ctx, state) = registered_mock("alpha");
    let state_dir = state.project_dir.as_path();
    let list = run_kt_agent(&["agent", "list", "--json"], &ctx.project_dir, state_dir);
    let usage = run_kt_agent(&["agent", "usage", "--json"], &ctx.project_dir, state_dir);
    // Fix pass (L5): assert the exit code BEFORE parsing, so a regression that
    // makes either command fail reports as a code mismatch naming stderr, rather
    // than an opaque `from_str().unwrap()` serde panic on empty stdout.
    assert_eq!(list.code, Some(0), "list --json; stderr={}", list.stderr);
    assert_eq!(usage.code, Some(0), "usage --json; stderr={}", usage.stderr);

    let list_doc: serde_json::Value = serde_json::from_str(&list.stdout).unwrap();
    let usage_doc: serde_json::Value = serde_json::from_str(&usage.stdout).unwrap();
    assert_eq!(
        list_doc["totals"], usage_doc["totals"],
        "usage --json totals must equal list --json totals exactly",
    );
    // And the named form's usage object equals that instance's `list` row usage.
    let named = run_kt_agent(
        &["agent", "usage", "alpha", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(named.code, Some(0), "stderr={}", named.stderr);
    let named_doc: serde_json::Value = serde_json::from_str(&named.stdout).unwrap();
    assert_eq!(
        list_doc["instances"][0]["usage"], named_doc["usage"],
        "usage <name> --json must equal that instance's list row usage",
    );
}

#[test]
fn usage_on_an_empty_fleet_prints_guidance_on_stderr_and_stays_pure_json() {
    // Fix pass (L3): `list` guides an operator with an empty Fleet; Fleet-wide
    // `usage` printed only all-zero totals, which reads as "instances that
    // consumed nothing" rather than "nothing registered". The hint is a NOTICE,
    // so it rides stderr in BOTH modes (AD-12) and `--json` stdout stays a pure,
    // parseable document.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();

    let human = run_kt_agent(&["agent", "usage"], &ctx.project_dir, state_dir);
    assert_eq!(human.code, Some(0), "stderr={}", human.stderr);
    assert!(
        human.stderr.contains("No Agent Instances registered yet"),
        "the empty-Fleet hint belongs on stderr; stderr={}",
        human.stderr
    );

    let json = run_kt_agent(&["agent", "usage", "--json"], &ctx.project_dir, state_dir);
    assert_eq!(json.code, Some(0), "stderr={}", json.stderr);
    assert!(
        json.stderr.contains("No Agent Instances registered yet"),
        "stderr={}",
        json.stderr
    );
    let doc: serde_json::Value = serde_json::from_str(&json.stdout)
        .unwrap_or_else(|e| panic!("stdout not pure JSON: {e}\n{}", json.stdout));
    assert_frozen_keys(&doc, &["schema_version", "totals"], "FleetUsageDocument");
    assert_frozen_keys(&doc["totals"], FLEET_TOTALS_KEYS, "FleetTotals");
}

#[test]
fn budget_view_key_set_is_frozen_on_the_json_wire() {
    // BudgetView only materializes once a budget is CONFIGURED (an un-budgeted
    // instance is an honest `null`). With a token-only budget and no Rate, the
    // dollar fields stay absent (skip_serializing_if) — the frozen minimal set.
    let (ctx, state) = registered_mock("alpha");
    let state_dir = state.project_dir.as_path();
    let set = run_kt_agent(
        &[
            "agent",
            "config",
            "set",
            "alpha",
            "budget.tokens.cumulative",
            "500",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert!(
        set.success,
        "config set should succeed; stderr={}",
        set.stderr
    );

    let run = run_kt_agent(
        &["agent", "show", "alpha", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    let doc: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout not pure JSON: {e}\n{}", run.stdout));
    let budget = &doc["instance"]["budget"];
    assert!(
        budget.is_object(),
        "a configured budget is an object: {doc}"
    );
    assert_frozen_keys(
        budget,
        &["breach_action", "cumulative_limit", "cumulative_remaining"],
        "BudgetView",
    );
    // Dollars are integer micros + a label on the wire, NEVER a `$` string
    // (AD-8/AD-14) — with no Rate the dollar dimension is absent entirely.
    assert!(
        !run.stdout.contains('$'),
        "no `$` on the wire: {}",
        run.stdout
    );
}

#[test]
fn list_json_priced_shape_key_sets_are_frozen() {
    // Fix pass (H2): the PRICED half of the wire — every fixture above registers
    // a Rate-less `mock`, so `skip_serializing_if` hid `UsageView`'s three dollar
    // fields, `FleetTotals`' two, and `BudgetView`'s five from EVERY frozen
    // assertion in this file. An adversarial pass added a `blended_rate` field to
    // `UsageView` (populated only in `with_dollars`) and all 69 tests passed.
    // With a Rate + both Cost Cap scopes configured, the maximal key-sets of all
    // three types serialize and are frozen here.
    let (ctx, state) = priced_instance("alpha");
    let run = run_kt_agent(
        &["agent", "list", "--json"],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);

    let doc: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout not pure JSON: {e}\n{}", run.stdout));
    assert_frozen_keys(
        &doc,
        &["instances", "schema_version", "totals"],
        "FleetListing",
    );
    assert_eq!(doc["schema_version"], serde_json::json!(2), "{doc}");
    assert_frozen_keys(
        &doc["totals"],
        FLEET_TOTALS_PRICED_KEYS,
        "FleetTotals (priced)",
    );

    let entry = &doc["instances"].as_array().expect("instances array")[0];
    assert_frozen_keys(entry, FLEET_ENTRY_KEYS, "FleetEntry");
    assert_frozen_keys(
        &entry["usage"],
        USAGE_VIEW_PRICED_KEYS,
        "UsageView (priced)",
    );
    assert_frozen_keys(
        &entry["budget"],
        BUDGET_VIEW_PRICED_KEYS,
        "BudgetView (priced)",
    );

    // On the wire, dollars are INTEGER MICROS + a label — never a `$` string
    // (AD-8/AD-14), in the priced shape just as in the unpriced one.
    assert!(
        !run.stdout.contains('$'),
        "no `$` on the wire: {}",
        run.stdout
    );
    assert!(
        entry["usage"]["cumulative_dollars"].is_number(),
        "dollars are integer micros: {entry}"
    );
    assert_eq!(
        entry["usage"]["estimate_label"],
        serde_json::json!("estimated"),
        "{entry}"
    );
}

#[test]
fn show_and_usage_json_priced_shape_key_sets_are_frozen() {
    // The same PRICED freeze (fix pass, H2) through the OTHER three documents:
    // `show --json`'s `ShowDocument.instance`, `usage <name> --json`'s
    // `UsageDocument.usage`, and `usage --json`'s `FleetUsageDocument.totals`.
    let (ctx, state) = priced_instance("alpha");
    let state_dir = state.project_dir.as_path();

    let show = run_kt_agent(
        &["agent", "show", "alpha", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(show.code, Some(0), "stderr={}", show.stderr);
    let show_doc: serde_json::Value = serde_json::from_str(&show.stdout)
        .unwrap_or_else(|e| panic!("stdout not pure JSON: {e}\n{}", show.stdout));
    assert_frozen_keys(&show_doc, &["instance", "schema_version"], "ShowDocument");
    assert_frozen_keys(
        &show_doc["instance"]["usage"],
        USAGE_VIEW_PRICED_KEYS,
        "UsageView (priced)",
    );
    assert_frozen_keys(
        &show_doc["instance"]["budget"],
        BUDGET_VIEW_PRICED_KEYS,
        "BudgetView (priced)",
    );

    let named = run_kt_agent(
        &["agent", "usage", "alpha", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(named.code, Some(0), "stderr={}", named.stderr);
    let named_doc: serde_json::Value = serde_json::from_str(&named.stdout)
        .unwrap_or_else(|e| panic!("stdout not pure JSON: {e}\n{}", named.stdout));
    assert_frozen_keys(
        &named_doc,
        &["instance", "schema_version", "usage"],
        "UsageDocument",
    );
    assert_frozen_keys(
        &named_doc["usage"],
        USAGE_VIEW_PRICED_KEYS,
        "UsageView (priced)",
    );

    let fleet = run_kt_agent(&["agent", "usage", "--json"], &ctx.project_dir, state_dir);
    assert_eq!(fleet.code, Some(0), "stderr={}", fleet.stderr);
    let fleet_doc: serde_json::Value = serde_json::from_str(&fleet.stdout)
        .unwrap_or_else(|e| panic!("stdout not pure JSON: {e}\n{}", fleet.stdout));
    assert_frozen_keys(
        &fleet_doc,
        &["schema_version", "totals"],
        "FleetUsageDocument",
    );
    assert_frozen_keys(
        &fleet_doc["totals"],
        FLEET_TOTALS_PRICED_KEYS,
        "FleetTotals (priced)",
    );

    // The priced documents agree with each other exactly (FR-22), just as the
    // unpriced ones do — one aggregate, several surfaces.
    assert_eq!(
        show_doc["instance"]["usage"], named_doc["usage"],
        "usage <name> --json must equal show --json's usage object",
    );
    for out in [&show.stdout, &named.stdout, &fleet.stdout] {
        assert!(!out.contains('$'), "no `$` on the wire: {out}");
    }
}

#[test]
fn config_get_json_document_key_set_and_schema_version_are_frozen() {
    // `config get --json` = ConfigDocument { schema_version, entries: [ConfigLeaf] }
    // with ConfigLeaf { key, value, source, unvalidated }. Its schema_version is
    // its OWN constant (1) — `config get` is not fleet-domain content.
    let (ctx, state) = registered_mock("alpha");
    let state_dir = state.project_dir.as_path();
    run_kt_agent(
        &["agent", "config", "set", "alpha", "model", "gpt-4"],
        &ctx.project_dir,
        state_dir,
    );

    let run = run_kt_agent(
        &["agent", "config", "get", "alpha", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);

    let doc: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout not pure JSON: {e}\n{}", run.stdout));
    assert_frozen_keys(&doc, &["entries", "schema_version"], "ConfigDocument");
    assert_eq!(doc["schema_version"], serde_json::json!(1), "{doc}");
    let leaf = &doc["entries"].as_array().expect("entries array")[0];
    assert_frozen_keys(
        leaf,
        &["key", "source", "unvalidated", "value"],
        "ConfigLeaf",
    );
}

// ---------------------------------------------------------------------------
// 6-6 — contract v1 negotiation at the REAL binary + the memory documents'
// frozen wire shapes (the ONE announced edit)
// ---------------------------------------------------------------------------

#[test]
fn register_incompatible_contract_manifest_exits_one_naming_both_versions_and_the_rule() {
    // FR-30 end-to-end at the REAL `kt` binary (the independent pass found the
    // kt-side negotiation proof was unit-only): registering a manifest whose
    // contract major differs from the engine's exits 1, the diagnostic names
    // BOTH versions and quotes the compatibility rule, stdout stays EMPTY
    // (diagnostics ride stderr, AD-12), and nothing is registered.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let manifest = VALID_MANIFEST.replace(
        "contract_version = \"1.0.0\"",
        "contract_version = \"2.1.0\"",
    );
    let m = manifest_dir(&ctx.project_dir, &manifest);

    let run = run_kt_agent(
        &["agent", "register", "m", "--manifest", m.to_str().unwrap()],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(1), "stderr={}", run.stderr);
    assert!(
        run.stdout.is_empty(),
        "no stdout on a rejected registration: {}",
        run.stdout
    );
    assert!(
        run.stderr.contains("2.1.0"),
        "names the manifest version: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("1.0.0"),
        "names the engine version: {}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("compatible iff the major versions match"),
        "quotes the compatibility rule: {}",
        run.stderr
    );

    // The instance genuinely does not exist (the load was refused, not half-done).
    let show = run_kt_agent(&["agent", "show", "m"], &ctx.project_dir, state_dir);
    assert_eq!(show.code, Some(3), "stdout={}", show.stdout);
}

#[test]
fn memory_attach_json_document_key_set_and_schema_version_are_frozen() {
    // Story 6-6's deferred wire surface, frozen at the contract v1 freeze.
    // MAXIMAL fixture: `filesystem` on a `mock` — the mock's builtin mapping
    // targets the reserved `memory.dir` key, so `declared` is TRUE and every
    // field materializes with its populated value (AI-64 rule). The kind +
    // guarantee ride the typed snake_case strings adopted verbatim
    // (`filesystem` / `managed_dir_byte_durable`); the schema_version is the
    // memory family's OWN constant (1).
    let (ctx, state) = registered_mock("demo");
    let state_dir = state.project_dir.as_path();

    let run = run_kt_agent(
        &[
            "agent",
            "memory",
            "attach",
            "demo",
            "--kind",
            "filesystem",
            "--json",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);

    // AD-14 purity: --json stdout is the document and NOTHING else.
    let doc: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout not pure JSON: {e}\n{}", run.stdout));
    assert_frozen_keys(&doc, MEMORY_ATTACH_KEYS, "MemoryAttachDocument");
    assert_eq!(doc["schema_version"], serde_json::json!(1), "{doc}");
    assert_eq!(doc["instance"], serde_json::json!("demo"), "{doc}");
    assert_eq!(doc["kind"], serde_json::json!("filesystem"), "{doc}");
    assert_eq!(
        doc["guarantee"],
        serde_json::json!("managed_dir_byte_durable"),
        "{doc}"
    );
    assert_eq!(
        doc["declared"],
        serde_json::json!(true),
        "the mock maps memory.dir, so the delivery fact is true: {doc}"
    );
    // DC-1: the dir is RECEIVED from the engine — it must genuinely exist.
    let dir = doc["dir"].as_str().expect("dir is a string").to_string();
    assert!(
        Path::new(&dir).is_dir(),
        "the managed directory exists at the reported path: {dir}"
    );
    assert!(
        dir.contains(
            &state_dir
                .join("agents")
                .join("demo")
                .to_string_lossy()
                .to_string()
        ),
        "the managed directory is inside the Agent Home: {dir}"
    );
    // No `$` anywhere on any --json wire (AD-8/AD-14).
    assert!(
        !run.stdout.contains('$'),
        "no `$` on the wire: {}",
        run.stdout
    );
}

#[test]
fn memory_attach_json_native_shape_is_frozen() {
    // The native twin (AI-64 maximal fixtures): kind `native`, guarantee
    // `home_persistence_only`, and `declared` FALSE — a native backing
    // delivers nothing, so there is no delivery to declare (never read as a
    // decline). The reported dir is the COMPUTED location only (nothing
    // created).
    let (ctx, state) = registered_mock("nat");
    let state_dir = state.project_dir.as_path();

    let run = run_kt_agent(
        &[
            "agent", "memory", "attach", "nat", "--kind", "native", "--json",
        ],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);

    let doc: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout not pure JSON: {e}\n{}", run.stdout));
    assert_frozen_keys(&doc, MEMORY_ATTACH_KEYS, "MemoryAttachDocument (native)");
    assert_eq!(doc["schema_version"], serde_json::json!(1), "{doc}");
    assert_eq!(doc["kind"], serde_json::json!("native"), "{doc}");
    assert_eq!(
        doc["guarantee"],
        serde_json::json!("home_persistence_only"),
        "{doc}"
    );
    assert_eq!(doc["declared"], serde_json::json!(false), "{doc}");
    let dir = doc["dir"].as_str().expect("dir is a string");
    assert!(
        !Path::new(dir).exists(),
        "a native backing must not create anything at the reported location: {dir}"
    );
}

#[test]
fn memory_detach_json_document_key_set_and_schema_version_are_frozen() {
    // The detach document is intentionally minimal: the versioned confirmation
    // that nothing is attached anymore. No path — kt never constructs the
    // managed-directory name itself (DC-1), and after a detach the engine
    // reports no attachment to quote.
    let (ctx, state) = registered_mock("demo");
    let state_dir = state.project_dir.as_path();
    let attach = run_kt_agent(
        &["agent", "memory", "attach", "demo", "--kind", "filesystem"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(attach.code, Some(0), "stderr={}", attach.stderr);

    let run = run_kt_agent(
        &["agent", "memory", "detach", "demo", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);

    let doc: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("stdout not pure JSON: {e}\n{}", run.stdout));
    assert_frozen_keys(&doc, MEMORY_DETACH_KEYS, "MemoryDetachDocument");
    assert_eq!(doc["schema_version"], serde_json::json!(1), "{doc}");
    assert_eq!(doc["instance"], serde_json::json!("demo"), "{doc}");
}

#[test]
fn memory_json_errors_stay_diagnostics_and_do_not_emit_documents() {
    // The failure case is NOT a document: an unknown instance still exits 3
    // with a stderr diagnostic, and stdout stays empty (AD-12/AD-14 — result
    // documents only on success).
    let (ctx, state) = registered_mock("demo");
    for args in [
        vec![
            "agent",
            "memory",
            "attach",
            "ghost",
            "--kind",
            "filesystem",
            "--json",
        ],
        vec!["agent", "memory", "detach", "ghost", "--json"],
    ] {
        let run = run_kt_agent(&args, &ctx.project_dir, state.project_dir.as_path());
        assert_eq!(run.code, Some(3), "args={args:?}; stderr={}", run.stderr);
        assert!(
            run.stdout.is_empty(),
            "no document on failure; args={args:?} stdout={}",
            run.stdout
        );
    }
    // A malformed name exits 2 (usage), also document-free.
    let run = run_kt_agent(
        &[
            "agent", "memory", "attach", "Bad Name", "--kind", "native", "--json",
        ],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert_eq!(run.code, Some(2), "stderr={}", run.stderr);
    assert!(
        run.stdout.is_empty(),
        "no document on failure: {}",
        run.stdout
    );
}

// ---------------------------------------------------------------------------
// 5.3 — `logs --json` NDJSON wire shape
// ---------------------------------------------------------------------------

#[test]
fn logs_json_on_an_empty_log_emits_zero_stdout_lines_not_an_empty_document() {
    // The NDJSON empty case: no retained output ⇒ NOTHING on stdout (not `[]`,
    // not `{}`), and still a clean exit 0. Cross-OS (no process spawn needed).
    let (ctx, state) = registered_mock("nat");
    let run = run_kt_agent(
        &["agent", "logs", "nat", "--json"],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);
    assert!(
        run.stdout.is_empty(),
        "an empty log must emit zero stdout bytes; stdout={}",
        run.stdout
    );
}

/// The ATTRIBUTED output log the engine's capture thread appends to and
/// `kt agent logs` reads back — `<state>/agents/<name>/logs/output.log`, JSON
/// Lines (one serialized `LogLine` per line). Distinct from the raw, legacy
/// `agent.log` next to it (see [`agent_log_path`]).
fn attributed_output_log_path(state_dir: &Path, name: &str) -> std::path::PathBuf {
    state_dir
        .join("agents")
        .join(name)
        .join("logs")
        .join("output.log")
}

/// Append `lines` to an instance's attributed output log, creating the log
/// directory if the instance has never been started (fix pass, H4).
///
/// This writes the SAME bytes the engine's capture thread writes — one
/// `serde_json`-serialized `LogLine` per line — so the CLI reads it back through
/// the ordinary `Supervisor::read_agent_log{,_since}` path with nothing stubbed.
/// It exists so the NDJSON WIRE CONTRACT (`logs --json`) can be asserted on ALL
/// THREE OSes: producing real captured output needs a live child that survives
/// its starting engine session, which only the Unix-only
/// `start_via_surviving_engine` harness can arrange.
fn append_captured_log_lines(state_dir: &Path, name: &str, lines: &[ktesio_engine::LogLine]) {
    use std::io::Write;
    let path = attributed_output_log_path(state_dir, name);
    std::fs::create_dir_all(path.parent().unwrap()).expect("create the instance log dir");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open the attributed output log");
    for line in lines {
        writeln!(file, "{}", serde_json::to_string(line).unwrap()).expect("append a log line");
    }
    file.flush().expect("flush the attributed output log");
}

/// One captured line, for [`append_captured_log_lines`].
fn captured(name: &str, stream: ktesio_engine::LogStream, text: &str) -> ktesio_engine::LogLine {
    ktesio_engine::LogLine::new(name, stream, text, "2026-07-20T12:00:00Z")
}

/// Assert every non-empty stdout line is ONE complete, compact `LogLine` JSON
/// object with the FROZEN key-set at the pinned `LOG_SCHEMA_VERSION`, and return
/// their `text` values in order. Shared by the one-shot and `--follow` NDJSON
/// tests so both hold the identical wire contract (story 4-3: the shape is the
/// same for both modes).
fn assert_ndjson_log_lines(stdout: &str, instance: &str) -> Vec<String> {
    let mut texts = Vec::new();
    for (index, raw) in stdout.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let line: serde_json::Value = serde_json::from_str(raw).unwrap_or_else(|e| {
            panic!("NDJSON line {index} is not valid JSON: {e}\n{raw}\nfull stdout:\n{stdout}")
        });
        assert_frozen_keys(
            &line,
            &["at", "instance", "schema_version", "stream", "text"],
            "LogLine",
        );
        // `LOG_SCHEMA_VERSION` pinned BY VALUE on the wire (not compared to the
        // constant it came from — that would be tautological).
        assert_eq!(line["schema_version"], serde_json::json!(1), "{line}");
        assert_eq!(line["instance"], serde_json::json!(instance), "{line}");
        let stream = line["stream"].as_str().expect("stream is a string");
        assert!(
            matches!(stream, "agent-out" | "agent-err" | "engine"),
            "unexpected stream token `{stream}`",
        );
        texts.push(line["text"].as_str().expect("text is a string").to_string());
    }
    // Stdout is PURE NDJSON — the human `<at> [<stream>] <text>` rendering never
    // leaks into it, and every notice rides stderr (AD-12/DC-3).
    assert!(
        !stdout.contains("[agent-out]")
            && !stdout.contains("[agent-err]")
            && !stdout.contains("[engine]"),
        "stdout must be pure NDJSON, not the human form; stdout={stdout}",
    );
    texts
}

#[test]
fn logs_json_wire_shape_is_frozen_ndjson_on_every_os() {
    // Fix pass (H4) — the CROSS-OS guard for the `logs --json` wire contract.
    //
    // Before this test the ONLY coverage of the NDJSON shape, the `LogLine`
    // key-set, the `LOG_SCHEMA_VERSION` VALUE, and stdout purity lived in
    // `logs_json_emits_..._in_append_order_unix`, which runtime-`return`s on
    // Windows — and a runtime early-return reports as PASSED, so CI showed green
    // there with ZERO signal that none of those assertions had run. This test
    // needs no live child (it seeds the same capture file the engine writes), so
    // it genuinely runs on all three OSes and gates the wire everywhere.
    let (ctx, state) = registered_mock("nat");
    let state_dir = state.project_dir.as_path();
    append_captured_log_lines(
        state_dir,
        "nat",
        &[
            captured("nat", ktesio_engine::LogStream::AgentOut, "first out"),
            captured("nat", ktesio_engine::LogStream::AgentErr, "second \"err\""),
            captured("nat", ktesio_engine::LogStream::Engine, "third engine"),
        ],
    );

    let run = run_kt_agent(
        &["agent", "logs", "nat", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);

    let texts = assert_ndjson_log_lines(&run.stdout, "nat");
    // APPEND ORDER — never re-sorted by the whole-second `at` (all three lines
    // deliberately share one timestamp, so a sort would be observable).
    assert_eq!(
        texts,
        vec!["first out", "second \"err\"", "third engine"],
        "NDJSON must preserve on-disk append order",
    );
    // Compact one-object-per-line (never `to_string_pretty`).
    assert_eq!(
        run.stdout.lines().filter(|l| !l.trim().is_empty()).count(),
        3,
        "one JSON object per stdout line; stdout={}",
        run.stdout
    );
    // And the human form reports the SAME lines in the SAME order.
    let human = run_kt_agent(&["agent", "logs", "nat"], &ctx.project_dir, state_dir);
    let human_texts: Vec<String> = human
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| l.split_once("] ").map(|(_, text)| text.to_string()))
        .collect();
    assert_eq!(texts, human_texts, "both modes share one order");
}

#[test]
fn logs_follow_json_emits_an_incremental_batch_as_valid_ndjson() {
    // Fix pass (H3) — `--follow --json` was NEVER exercised. The whole suite had
    // exactly one `--follow` invocation and it ran in HUMAN mode, so an
    // adversarial pass could change the follow loop's
    // `emit_log_lines(&new_lines, json)` to `emit_log_lines(&new_lines, false)`
    // — making every INCREMENTAL follow batch emit human text into a stream the
    // caller is parsing as NDJSON — and all 33 logs tests still passed.
    //
    // This test spawns the real `--follow --json`, waits (by POLLING committed
    // output, never a fixed sleep) until the initial backlog has been emitted,
    // and only THEN appends a new captured line. The appended line therefore
    // cannot be part of the already-read backlog: it can only reach stdout
    // through a follow batch. Cross-OS: a `registered` instance is not `running`,
    // so follow emits its batch and then exits cleanly on its own.
    let (ctx, state) = registered_mock("nat");
    let state_dir = state.project_dir.as_path();
    append_captured_log_lines(
        state_dir,
        "nat",
        &[captured(
            "nat",
            ktesio_engine::LogStream::AgentOut,
            "backlog line",
        )],
    );

    // Redirect the follow child's stdout to a FILE so the parent can poll it for
    // committed output while the child is still running.
    let out_path = ctx.project_dir.join("follow-stdout.ndjson");
    let err_path = ctx.project_dir.join("follow-stderr.txt");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_kt"))
        .args(["agent", "logs", "nat", "--follow", "--json"])
        .current_dir(&ctx.project_dir)
        .env("KTESIO_NO_UPDATE_CHECK", "1")
        .env("KTESIO_STATE_DIR", state_dir)
        .stdout(std::fs::File::create(&out_path).expect("create follow stdout file"))
        .stderr(std::fs::File::create(&err_path).expect("create follow stderr file"))
        .spawn()
        .expect("spawn kt agent logs --follow --json");

    // POLL for the backlog to be committed to stdout (the determinism rule: never
    // sleep to await state). Once ANY line is visible the one-shot read has
    // already returned, so the backlog it dumped is fixed and cannot include what
    // we append next.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let seen = std::fs::read_to_string(&out_path).unwrap_or_default();
        if seen.contains("backlog line") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "follow --json never emitted its initial backlog; stdout so far={seen}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // The INCREMENTAL line — appended strictly after the backlog was emitted.
    append_captured_log_lines(
        state_dir,
        "nat",
        &[captured(
            "nat",
            ktesio_engine::LogStream::Engine,
            "incremental line",
        )],
    );

    // Follow must end on its own (the instance is not running) — never hang.
    let exit_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            std::time::Instant::now() < exit_deadline,
            "kt agent logs --follow --json must not hang on a non-running instance"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(
        status.code(),
        Some(0),
        "follow must exit 0; stderr={}",
        std::fs::read_to_string(&err_path).unwrap_or_default()
    );

    let stdout = std::fs::read_to_string(&out_path).expect("read follow stdout");
    // EVERY line — backlog AND the incremental batch — is still valid NDJSON with
    // the frozen key-set, and no human rendering leaked in.
    let texts = assert_ndjson_log_lines(&stdout, "nat");
    assert_eq!(
        texts,
        vec!["backlog line", "incremental line"],
        "the incremental batch must be emitted, as NDJSON, in append order",
    );
    // The exit note is a NOTICE — stderr only (AD-12/DC-3), never mixed into the
    // NDJSON stream a caller is parsing.
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    assert!(
        stderr.contains("no further output"),
        "the honest follow-exit note belongs on stderr; stderr={stderr}"
    );
}

#[test]
fn logs_json_survives_a_consumer_that_stops_reading_and_still_exits_zero() {
    // Fix pass (M1): `kt agent logs --json | head -5` used to PANIC. Rust ignores
    // SIGPIPE, so `println!` hit `ErrorKind::BrokenPipe`, unwrapped, and aborted
    // with exit 101 — a code outside the frozen table `docs/commands.md`
    // documents ("Every `kt` command returns one of these numeric exit codes").
    // A closed downstream pipe is not an error: the consumer got what it asked
    // for, so the command now ends cleanly with 0.
    //
    // Cross-OS and no shell needed: the parent closes the read end of the pipe
    // immediately and enough output is seeded to far exceed any pipe buffer, so
    // the child provably writes into a closed pipe.
    let (ctx, state) = registered_mock("nat");
    let state_dir = state.project_dir.as_path();
    let lines: Vec<ktesio_engine::LogLine> = (0..5_000)
        .map(|i| {
            captured(
                "nat",
                ktesio_engine::LogStream::AgentOut,
                &format!("line {i} — padding to push well past any pipe buffer"),
            )
        })
        .collect();
    append_captured_log_lines(state_dir, "nat", &lines);

    for json in [true, false] {
        let mut args = vec!["agent", "logs", "nat"];
        if json {
            args.push("--json");
        }
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_kt"))
            .args(&args)
            .current_dir(&ctx.project_dir)
            .env("KTESIO_NO_UPDATE_CHECK", "1")
            .env("KTESIO_STATE_DIR", state_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn kt agent logs");
        // Close the read end at once — every subsequent child write fails with
        // BrokenPipe, exactly as `| head -5` does once head has its lines.
        drop(child.stdout.take());

        let status = child.wait().expect("wait for kt agent logs");
        assert_eq!(
            status.code(),
            Some(0),
            "a closed stdout consumer must exit 0 (never a 101 panic); args={args:?}",
        );
    }
}

#[test]
fn logs_json_emits_newline_delimited_self_versioned_loglines_in_append_order_unix() {
    // AC1/DC-2/DC-3 for the net-new `logs --json`, against a REAL captured log:
    // every stdout line is ONE complete, compact `LogLine` JSON object carrying
    // its OWN schema_version (LOG_SCHEMA_VERSION = 1, reused — no new constant),
    // the key-set is frozen, stdout is pure NDJSON (no note leaks), and the ORDER
    // matches the human form exactly (append order — never timestamp-sorted,
    // story 4-2 AC-G).
    //
    // Runtime-skip on Windows: mirrors the sibling `logs_*` CLI tests — the
    // cross-lifetime survival this needs cannot be simulated there. NO `#[cfg]`
    // (data-driven; this file is outside the backends allowlist).
    //
    // RENAMED with the `_unix` suffix (fix pass, H4 — the repo's existing
    // convention, cf. `pause_prints_paused_state_and_exits_zero_guaranteed_unix`)
    // because a runtime early-return reports as PASSED: the old name promised
    // coverage this test does not provide on Windows. The OS-INDEPENDENT half of
    // its contract — the NDJSON shape, the frozen `LogLine` key-set, the pinned
    // `LOG_SCHEMA_VERSION`, stdout purity, and append order — now runs on all
    // three OSes in `logs_json_wire_shape_is_frozen_ndjson_on_every_os`. What
    // remains here, and only here, is the end-to-end proof over output captured
    // by the real engine rather than seeded onto disk.
    if ktesio_engine::OsId::current() == ktesio_engine::OsId::Windows {
        return;
    }
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let m = fake_agent_manifest_with_interaction(
        &ctx.project_dir,
        &["--heartbeat-ms", "40", "--linger-ms", "600000"],
        "guaranteed",
    );
    run_kt_agent(
        &[
            "agent",
            "register",
            "svc",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    start_via_surviving_engine(state_dir, "svc");
    let stopped = run_kt_agent(&["agent", "stop", "svc"], &ctx.project_dir, state_dir);
    assert!(
        stopped.success,
        "stop should succeed; stderr={}",
        stopped.stderr
    );

    let run = run_kt_agent(
        &["agent", "logs", "svc", "--json"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);

    let lines: Vec<&str> = run
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert!(
        !lines.is_empty(),
        "the retained history must produce NDJSON lines; stdout={}",
        run.stdout
    );
    let mut texts = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        let line: serde_json::Value = serde_json::from_str(raw)
            .unwrap_or_else(|e| panic!("NDJSON line {index} is not valid JSON: {e}\n{raw}"));
        // The FROZEN LogLine key-set + its own schema version (reused, not new).
        assert_frozen_keys(
            &line,
            &["at", "instance", "schema_version", "stream", "text"],
            "LogLine",
        );
        assert_eq!(line["schema_version"], serde_json::json!(1), "{line}");
        assert_eq!(line["instance"], serde_json::json!("svc"));
        // `stream` is the kebab-case AD-12 attribution vocabulary.
        let stream = line["stream"].as_str().expect("stream is a string");
        assert!(
            matches!(stream, "agent-out" | "agent-err" | "engine"),
            "unexpected stream token `{stream}`",
        );
        texts.push(line["text"].as_str().expect("text is a string").to_string());
    }

    // Stdout is PURE NDJSON: the human `<at> [<stream>]` rendering never leaks
    // into it, and notices ride stderr (AD-12/DC-3).
    assert!(
        !run.stdout.contains("[agent-out]") && !run.stdout.contains("[engine]"),
        "stdout must be pure NDJSON, not the human form; stdout={}",
        run.stdout
    );

    // APPEND ORDER: the NDJSON `text` sequence equals the human form's sequence
    // (same read, same order — never re-sorted by the whole-second `at`).
    let human = run_kt_agent(&["agent", "logs", "svc"], &ctx.project_dir, state_dir);
    let human_texts: Vec<String> = human
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        // Human form is `<at> [<stream>] <text>` — take everything after "] ".
        .filter_map(|l| l.split_once("] ").map(|(_, text)| text.to_string()))
        .collect();
    assert_eq!(
        texts, human_texts,
        "NDJSON order must match the human append order exactly",
    );
}

// ---------------------------------------------------------------------------
// 5.2 — the numeric exit-code contract, end-to-end through the real binary
// ---------------------------------------------------------------------------
//
// The exhaustive diagnostic→code mapping (including code 5 and code 6, whose
// triggering conditions need a live/stuck child process) is pinned
// deterministically and cross-OS by the classifier unit tests in
// `crates/kt/src/exit_code.rs`. The tests below prove the OTHER half: that the
// classifier is actually WIRED to the process exit status through `main`, for
// each condition reachable without a real spawned agent.

#[test]
fn a_successful_read_command_exits_with_the_success_code() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let run = run_kt_agent(
        &["agent", "list"],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert_eq!(run.code, Some(0), "success is 0; stderr={}", run.stderr);
}

#[test]
fn agent_show_missing_instance_exits_with_the_not_found_code() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    for args in [
        vec!["agent", "show", "ghost"],
        vec!["agent", "usage", "ghost"],
        vec!["agent", "logs", "ghost"],
    ] {
        let run = run_kt_agent(&args, &ctx.project_dir, state_dir);
        assert_eq!(
            run.code,
            Some(3),
            "{args:?} must exit 3 (not found); stderr={}",
            run.stderr
        );
    }
}

#[test]
fn a_missing_manifest_exits_with_the_not_found_code() {
    let ctx = TestContext::new();
    let state = TestContext::new();
    let missing = ctx.project_dir.join("no-such-adapter-dir");
    let run = run_kt_agent(
        &[
            "agent",
            "register",
            "m",
            "--manifest",
            missing.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert_eq!(run.code, Some(3), "stderr={}", run.stderr);
}

#[test]
fn invalid_invocations_exit_with_the_usage_code() {
    // Code 2 covers BOTH clap's own parse/usage errors (unchanged — clap exits
    // 2 itself) and the modeled usage diagnostics.
    let (ctx, state) = registered_mock("alpha");
    let state_dir = state.project_dir.as_path();
    let cases: Vec<(&str, Vec<&str>)> = vec![
        (
            "duplicate name",
            vec!["agent", "register", "alpha", "--kind", "mock"],
        ),
        (
            "invalid name",
            vec!["agent", "register", "BadName", "--kind", "mock"],
        ),
        (
            "unknown kind",
            vec!["agent", "register", "b", "--kind", "no-such-kind"],
        ),
        (
            "unknown config key",
            vec!["agent", "config", "set", "alpha", "nope", "1"],
        ),
        // clap: an unknown flag, and a missing required adapter selector.
        ("clap unknown flag", vec!["agent", "list", "--nope"]),
        ("clap missing adapter", vec!["agent", "register", "z"]),
    ];
    for (what, args) in cases {
        let run = run_kt_agent(&args, &ctx.project_dir, state_dir);
        assert_eq!(
            run.code,
            Some(2),
            "{what} ({args:?}) must exit 2 (usage); stderr={}",
            run.stderr
        );
    }
}

#[test]
fn a_malformed_instance_name_exits_with_the_usage_code_on_every_read_command() {
    // Fix pass (M2), ratified "make it uniformly 2". A malformed name is a USAGE
    // error (2) on every command, not "not found" (3) on some of them.
    //
    // `logs`/`stop`/`show` pass the raw name into an engine call that validates it
    // (`InstanceName::new` → `InvalidName` → 2). `usage <name>` and `show --json`
    // instead resolved the instance with a linear `find` over `fleet()` and then
    // SYNTHESIZED `RegistryError::NotFound`, so the SAME input exited 3 there —
    // two different codes for one condition, which is exactly what a scripted
    // caller cannot branch on. Both now validate first.
    let (ctx, state) = registered_mock("alpha");
    let state_dir = state.project_dir.as_path();
    for bad in ["Bad Name", "UPPER", "-leading"] {
        for args in [
            vec!["agent", "usage", bad],
            vec!["agent", "usage", bad, "--json"],
            vec!["agent", "show", bad],
            vec!["agent", "show", bad, "--json"],
            vec!["agent", "logs", bad],
            vec!["agent", "stop", bad],
        ] {
            let run = run_kt_agent(&args, &ctx.project_dir, state_dir);
            assert_eq!(
                run.code,
                Some(2),
                "{args:?} must exit 2 (usage), never 3 (not found); stderr={}",
                run.stderr
            );
        }
    }
    // And a WELL-FORMED but unregistered name still exits 3 — the validation
    // above must not swallow the genuine not-found case.
    for args in [
        vec!["agent", "usage", "ghost", "--json"],
        vec!["agent", "show", "ghost", "--json"],
    ] {
        let run = run_kt_agent(&args, &ctx.project_dir, state_dir);
        assert_eq!(
            run.code,
            Some(3),
            "{args:?} must still exit 3 (not found); stderr={}",
            run.stderr
        );
    }
}

#[test]
fn an_operation_on_a_wrong_state_instance_exits_with_the_invalid_state_code() {
    // A never-started instance cannot be stopped/paused/resumed — the uniform
    // invalid-transition class (code 4).
    let (ctx, state) = registered_mock("alpha");
    let state_dir = state.project_dir.as_path();
    for args in [
        vec!["agent", "stop", "alpha"],
        vec!["agent", "pause", "alpha"],
        vec!["agent", "resume", "alpha"],
    ] {
        let run = run_kt_agent(&args, &ctx.project_dir, state_dir);
        assert_eq!(
            run.code,
            Some(4),
            "{args:?} must exit 4 (invalid state); stderr={}",
            run.stderr
        );
    }
}

#[test]
fn an_internal_failure_exits_with_the_general_code() {
    // A structurally INVALID manifest (present, parses, but fails validation)
    // is the general/internal class — code 1, which is also the catch-all.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let dir = ctx.project_dir.join("bad-adapter");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("adapter.toml"),
        "contract_version = \"1.0.0\"\n[adapter]\nkind = \"x\"\n",
    )
    .unwrap();
    let run = run_kt_agent(
        &[
            "agent",
            "register",
            "bad",
            "--manifest",
            dir.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert_eq!(
        run.code,
        Some(1),
        "an invalid manifest must exit 1 (general); stderr={}",
        run.stderr
    );
}

#[test]
fn help_and_version_still_exit_zero() {
    // clap's `0` for `--help`/`--version` is preserved (never reclassified).
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    for args in [vec!["--help"], vec!["--version"], vec!["agent", "--help"]] {
        let run = run_kt_agent(&args, &ctx.project_dir, state_dir);
        assert_eq!(
            run.code,
            Some(0),
            "{args:?} must exit 0; stderr={}",
            run.stderr
        );
    }
}

// ---- Story 5-1: `kt agent memory attach | detach` ----
//
// DC-1 note: this file NEVER joins the managed directory's name itself — the
// path arrives printed on stdout FROM the engine, and tests assert against that
// engine-reported path (the same discipline the shipping code follows).

/// Seed a Memory Backing row of an arbitrary `kind` directly into the state DB
/// (the `force_state_running` technique, extended to the v5 table). Used to
/// reach the kind-conflict guard, which the `--kind filesystem`-only CLI cannot
/// otherwise produce.
fn force_attach_kind(state_dir: &Path, name: &str, kind: &str) {
    let conn = rusqlite::Connection::open(state_db(state_dir)).expect("open state db");
    let id: i64 = conn
        .query_row(
            "SELECT id FROM agent_instances WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .expect("instance row exists");
    let affected = conn
        .execute(
            "INSERT INTO agent_memory_backing (instance_id, kind, attached_at) \
             VALUES (?1, ?2, '2026-08-23T00:00:00Z') \
             ON CONFLICT(instance_id) DO UPDATE SET kind = excluded.kind",
            rusqlite::params![id, kind],
        )
        .expect("seed backing row");
    assert_eq!(affected, 1, "expected to seed exactly one backing row");
}

#[test]
fn memory_attach_prints_the_engine_managed_path_and_creates_it() {
    // AC1 at the CLI + DC-1: the managed path is RECEIVED from the engine
    // (printed on stdout), and the directory genuinely exists at that path.
    // This test never constructs the path itself.
    let (ctx, state) = registered_mock("demo");
    let state_dir = state.project_dir.as_path();

    let run = run_kt_agent(
        &["agent", "memory", "attach", "demo", "--kind", "filesystem"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(
        run.code,
        Some(0),
        "attach should exit 0; stderr={}",
        run.stderr
    );
    assert!(run.stdout.contains("Attached"), "stdout={}", run.stdout);
    assert!(
        run.stdout.contains("filesystem"),
        "names the kind; stdout={}",
        run.stdout
    );
    assert!(
        run.stdout.contains("demo"),
        "names the instance; stdout={}",
        run.stdout
    );

    // The LAST stdout line is the engine-computed path — it must exist on disk.
    let reported = run
        .stdout
        .lines()
        .last()
        .expect("attach prints the managed path")
        .trim()
        .to_string();
    assert!(
        Path::new(&reported).is_dir(),
        "the engine-reported managed directory must exist: {reported}"
    );
    // And it lives inside the instance's Agent Home (which register printed).
    assert!(
        reported.contains(
            &state_dir
                .join("agents")
                .join("demo")
                .to_string_lossy()
                .to_string()
        ),
        "the managed directory is inside the Agent Home: {reported}"
    );

    // Re-attaching the SAME kind is an idempotent success (A-6).
    let again = run_kt_agent(
        &["agent", "memory", "attach", "demo", "--kind", "filesystem"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(
        again.code,
        Some(0),
        "idempotent re-attach; stderr={}",
        again.stderr
    );

    // Detaching reports metadata-only semantics on stdout.
    let detach = run_kt_agent(
        &["agent", "memory", "detach", "demo"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(detach.code, Some(0), "detach; stderr={}", detach.stderr);
    assert!(
        detach.stdout.contains("Detached") && detach.stdout.contains("remain"),
        "detach names what happened AND that data stays; stdout={}",
        detach.stdout
    );
    assert!(
        Path::new(&reported).is_dir(),
        "detach leaves the managed directory on disk"
    );
}

#[test]
fn an_unknown_memory_kind_exits_with_the_usage_code() {
    // DC-4: an unsupported `--kind` token is a USAGE error (2), naming BOTH
    // accepted values (story 5-2 widened the accepted set to filesystem+native;
    // the diagnostic tracks the set).
    let (ctx, state) = registered_mock("demo");
    let run = run_kt_agent(
        &["agent", "memory", "attach", "demo", "--kind", "teleporting"],
        &ctx.project_dir,
        state.project_dir.as_path(),
    );
    assert_eq!(run.code, Some(2), "stderr={}", run.stderr);
    assert!(
        run.stderr.contains("filesystem") && run.stderr.contains("native"),
        "names both accepted values; stderr={}",
        run.stderr
    );
}

#[test]
fn memory_attach_native_is_metadata_only_and_states_the_delegation_boundary() {
    // Story 5-2, AC1 at the CLI + DC-6: attaching `native` succeeds, prints the
    // NFR-7 boundary sentence (delegation, Agent Home persistence only), and
    // prints a path that is NOT created on disk (the computed location only).
    // The LAST stdout line stays the engine-reported path (DC-1 discipline).
    let (ctx, state) = registered_mock("nat");
    let state_dir = state.project_dir.as_path();

    let run = run_kt_agent(
        &["agent", "memory", "attach", "nat", "--kind", "native"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);
    assert!(
        run.stdout.contains("Attached") && run.stdout.contains("native"),
        "names the kind; stdout={}",
        run.stdout
    );
    assert!(
        run.stdout.contains("only Agent Home persistence"),
        "states the delegation boundary; stdout={}",
        run.stdout
    );

    // The reported location is the COMPUTED memory dir inside the Agent Home,
    // and it was NOT created.
    let reported = run
        .stdout
        .lines()
        .last()
        .expect("attach prints the computed location")
        .trim()
        .to_string();
    assert!(
        reported.contains(
            &state_dir
                .join("agents")
                .join("nat")
                .to_string_lossy()
                .to_string()
        ),
        "the reported location is inside the Agent Home: {reported}"
    );
    assert!(
        !Path::new(&reported).exists(),
        "a native backing must not create anything at the reported location"
    );

    // Idempotent re-attach of the same kind succeeds (A-6) and still creates
    // nothing.
    let again = run_kt_agent(
        &["agent", "memory", "attach", "nat", "--kind", "native"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(again.code, Some(0), "stderr={}", again.stderr);
    assert!(!Path::new(&reported).exists());

    // The kind-conflict guard works in BOTH directions now that the CLI can
    // request either kind natively (A-6): native over an attached filesystem → 4.
}

#[test]
fn memory_attach_filesystem_states_the_managed_directory_guarantee() {
    // Story 5-2, DC-6: the filesystem attach confirmation ALSO states its NFR-7
    // boundary — the managed-directory guarantee — so both kinds' outputs name
    // what is guaranteed versus delegated.
    let (ctx, state) = registered_mock("fs");
    let state_dir = state.project_dir.as_path();

    let run = run_kt_agent(
        &["agent", "memory", "attach", "fs", "--kind", "filesystem"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);
    assert!(
        run.stdout.contains("managed directory") && run.stdout.contains("byte-identically"),
        "states the managed-directory guarantee; stdout={}",
        run.stdout
    );
}

#[test]
fn a_malformed_name_on_memory_commands_exits_with_the_usage_code() {
    // DC-4 + the shipped 4-3 M2 convention: validate the name FIRST so a
    // malformed name exits 2 uniformly (never 3 via a lookup miss).
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    for args in [
        vec![
            "agent",
            "memory",
            "attach",
            "Bad Name",
            "--kind",
            "filesystem",
        ],
        vec!["agent", "memory", "detach", "Bad Name"],
    ] {
        let run = run_kt_agent(&args, &ctx.project_dir, state_dir);
        assert_eq!(
            run.code,
            Some(2),
            "{args:?} must exit 2 (usage); stderr={}",
            run.stderr
        );
    }
}

#[test]
fn memory_commands_on_an_unregistered_instance_exit_with_the_not_found_code() {
    // DC-4: unregistered instance → 3.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    for args in [
        vec!["agent", "memory", "attach", "ghost", "--kind", "filesystem"],
        vec!["agent", "memory", "detach", "ghost"],
    ] {
        let run = run_kt_agent(&args, &ctx.project_dir, state_dir);
        assert_eq!(
            run.code,
            Some(3),
            "{args:?} must exit 3 (not found); stderr={}",
            run.stderr
        );
    }
}

#[test]
fn memory_attach_and_detach_on_a_running_instance_exit_with_the_invalid_state_code() {
    // AC3 at the CLI (DC-4): no hot-swap → 4, with NO force escape. Running is
    // seeded directly into the DB (no live process needed — the guard is pure
    // persisted-state validation, DC-3).
    let (ctx, state) = registered_mock("live");
    let state_dir = state.project_dir.as_path();
    force_state_running(state_dir, "live");

    let attach = run_kt_agent(
        &["agent", "memory", "attach", "live", "--kind", "filesystem"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(attach.code, Some(4), "stderr={}", attach.stderr);
    assert!(
        attach.stderr.contains("hot-swapped") || attach.stderr.contains("terminal"),
        "names the remediation; stderr={}",
        attach.stderr
    );

    let detach = run_kt_agent(
        &["agent", "memory", "detach", "live"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(detach.code, Some(4), "stderr={}", detach.stderr);
}

#[test]
fn attaching_a_different_kind_than_the_attached_one_exits_with_the_invalid_state_code() {
    // A-6 / DC-4: kinds never conflict-overwrite — detach first → 4.
    let (ctx, state) = registered_mock("demo");
    let state_dir = state.project_dir.as_path();
    force_attach_kind(state_dir, "demo", "native");

    let run = run_kt_agent(
        &["agent", "memory", "attach", "demo", "--kind", "filesystem"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(4), "stderr={}", run.stderr);
    assert!(
        run.stderr.contains("'native'") && run.stderr.contains("detach"),
        "names both kinds + the remediation; stderr={}",
        run.stderr
    );
}

#[test]
fn attaching_native_over_an_attached_filesystem_exits_with_the_invalid_state_code() {
    // Story 5-2: the CLI can now request `native` directly, so exercise the
    // conflict guard in the OTHER direction too (filesystem attached, native
    // requested → 4, detach remediation, no hot-swap).
    let (ctx, state) = registered_mock("demo");
    let state_dir = state.project_dir.as_path();
    force_attach_kind(state_dir, "demo", "filesystem");

    let run = run_kt_agent(
        &["agent", "memory", "attach", "demo", "--kind", "native"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(run.code, Some(4), "stderr={}", run.stderr);
    assert!(
        run.stderr.contains("'filesystem'") && run.stderr.contains("detach"),
        "names both kinds + the remediation; stderr={}",
        run.stderr
    );
}

#[test]
fn a_start_with_an_attached_but_unmapped_memory_backing_says_so_and_still_succeeds() {
    // DC-10 end to end through the real binary: a manifest adapter declaring NO
    // mapping for the reserved key gets ONE stderr notice naming the key — and
    // the start STILL succeeds (exit 0). Delivery is offered, not imposed; the
    // engine is never silent about it.
    let ctx = TestContext::new();
    let state = TestContext::new();
    let state_dir = state.project_dir.as_path();
    let m = fake_agent_manifest(&ctx.project_dir, &["--linger-ms", "600000"]);

    run_kt_agent(
        &[
            "agent",
            "register",
            "svc",
            "--manifest",
            m.to_str().unwrap(),
        ],
        &ctx.project_dir,
        state_dir,
    );
    let attach = run_kt_agent(
        &["agent", "memory", "attach", "svc", "--kind", "filesystem"],
        &ctx.project_dir,
        state_dir,
    );
    assert_eq!(attach.code, Some(0), "stderr={}", attach.stderr);

    let start = run_kt_agent(&["agent", "start", "svc"], &ctx.project_dir, state_dir);
    assert_eq!(
        start.code,
        Some(0),
        "unmapped delivery must not fail; stderr={}",
        start.stderr
    );
    assert!(
        start.stderr.contains("memory.dir"),
        "the notice names the reserved key; stderr={}",
        start.stderr
    );
    assert!(
        !start.stdout.contains("memory.dir"),
        "the notice is a diagnostic (stderr), never stdout",
    );
}
