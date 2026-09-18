//! The story 6-4 HERMES TCK pass (AC line 2): the conformance Test Kit runs
//! against the REAL shipping `hermes` builtin — a native adapter registered by
//! kind — and every section APPLICABLE to Hermes' actual declaration must pass,
//! while the EngineObserved metering section reads `not_applicable` because
//! Hermes declares SelfReported (CP-d). No section may demand EngineObserved of
//! a SelfReported adapter; conversely the BestEffort pause declaration is still
//! APPLICABLE — the harness must DEMONSTRATE the best-effort path
//! (`pause-best-effort` / `resume-best-effort` cause tags), never skip it.
//!
//! ## Isolation (the recorded/sandboxed hermes.rs pattern — never a gateway)
//!
//! The declared launch (`hermes gateway run --external-supervisor`) resolves
//! through PATH to the committed `hermes_shim` launcher COPIED as
//! `<tmp>/hermes<EXE_SUFFIX>` (with `fake_agent` beside it), scripted via
//! `HERMES_SHIM_ARGS` — the same no-network sandbox `tests/hermes.rs` uses.
//! The harness's own probe fixtures exec `fake_agent` by ABSOLUTE path, so the
//! PATH shim only ever captures the hermes-kind subject.
//!
//! **PATH discipline** (process-global, `unsafe` under edition 2024): the
//! tests here that mutate the environment do so ONCE at their start, before
//! any child is spawned, and RESTORE both `PATH` and `HERMES_SHIM_ARGS` at
//! teardown. Under nextest each test is its own process; under plain
//! `cargo test` a binary's tests run on parallel threads, so every
//! env-mutating test here holds the shared `PATH_LOCK` mutex for its whole
//! mutate→spawn→restore journey (the old "exactly one env-mutating test per
//! binary" rule, kept honest when the subject-delivery test joined).
//!
//! ## Expected report shape (derived from Hermes' declaration, not hardcoded
//! per adapter — the harness derives it from the registered snapshot; these
//! assertions PIN it for the shipping adapter):
//!
//! * `capability_edges`, `lifecycle` (incl. the crash leg), `pause`
//!   (BestEffort — demonstrated, not skipped), `metering_self_reported`,
//!   `memory` (Hermes declares `memory.dir` → `HERMES_HOME`), `interaction`
//!   (Guaranteed) → **pass**.
//! * `metering_engine_observed` → **not_applicable** (SelfReported).
//! * `config_mapping` → **not_applicable**: the launch is CODE-declared
//!   (contract argv, no `--dump <path>` seam the harness could author), so
//!   delivered config has no observable artifact for a native subject; the
//!   reserved-key delivery Hermes DOES declare is proven by `memory`.

use std::path::PathBuf;

use hemaka_conformance::{
    run_conformance, section_ids, ConformanceReport, SectionResult, TckAdapter,
};
use hemaka_engine::{AdapterRef, Engine, MemoryBackingKind};
use tempfile::TempDir;

/// Copy the committed `hermes_shim` launcher onto PATH as `hermes<EXE_SUFFIX>`
/// and return the shim path — the `tests/hermes.rs` `install_shim` shape (the
/// shim resolves its script target beside ITSELF, so `fake_agent` is copied
/// into the same directory).
fn install_shim(shim_dir: &TempDir) -> PathBuf {
    let exe = std::env::current_exe().expect("locate the running test executable");
    let mut dir = exe;
    dir.pop(); // drop the test-bin file name
    if dir.ends_with("deps") {
        dir.pop(); // drop `deps`
    }
    let candidate = dir.join(format!("hermes_shim{}", std::env::consts::EXE_SUFFIX));
    let source = if candidate.exists() {
        candidate
    } else {
        // Not built by this harness — build it on demand (the same fallback
        // shape as `fake_agent_bin`'s; note this file is an INTEGRATION TEST
        // target, so its lines are not part of the coverage denominator —
        // no tarpaulin cfg is needed here, unlike the lib-side fallback).
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = std::process::Command::new(cargo)
            .args(["build", "-p", "hemaka-conformance", "--bin", "hermes_shim"])
            .env_remove("RUSTC_WRAPPER") // a shimmed PATH must not break the build
            .status()
            .expect("run cargo for hermes_shim");
        assert!(status.success(), "on-demand hermes_shim build failed");
        candidate
    };
    let shim = shim_dir
        .path()
        .join(format!("hermes{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&source, &shim).expect("copy hermes_shim onto PATH");
    // The shim re-execs fake_agent beside ITSELF (current_exe anchoring).
    let agent = hemaka_conformance::fake_agent_bin();
    std::fs::copy(
        &agent,
        shim_dir
            .path()
            .join(format!("fake_agent{}", std::env::consts::EXE_SUFFIX)),
    )
    .expect("copy fake_agent beside the shim");
    shim
}

/// The Hermes TCK pass. One function owns the whole environment-mutating
/// journey (see the module doc's PATH discipline).
#[test]
fn hermes_tck_passes_every_section_applicable_to_its_declaration() {
    // Serialize with this binary's OTHER PATH-mutating test (plain `cargo
    // test` runs a binary's tests on parallel threads; see PATH_LOCK).
    let _env_guard = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // ---- Sandbox setup: the PATH shim + the shim script (linger so the
    // harness's lifecycle/pause sections can drive the subject). The guard
    // restores the env in Drop — panic-safe by construction.
    let shim_dir = TempDir::new().unwrap();
    let _shim = install_shim(&shim_dir);
    let _env = unsafe { EnvRestore::install(&shim_dir, "--linger-ms 600000") };

    // THE harness call: register the shipping hermes builtin with a fresh
    // engine and run every section.
    let report: ConformanceReport = run_conformance(&TckAdapter::Native("hermes".to_string()));

    // ---- The report contract.
    assert_eq!(report.adapter_kind, "hermes");
    assert_eq!(report.sections.len(), 8, "the report must be complete");

    // Everything applicable to the declaration PASSED.
    assert!(
        report.is_conformant(),
        "hermes must conform: failures = {:?}",
        report.failures()
    );

    // The demonstrated sections (each proves real behavior, never a skip).
    for id in [
        section_ids::CAPABILITY_EDGES,
        section_ids::LIFECYCLE,
        section_ids::PAUSE,
        section_ids::METERING_SELF_REPORTED,
        section_ids::MEMORY,
        section_ids::INTERACTION,
    ] {
        assert_eq!(
            report.section(id),
            Some(&SectionResult::Pass),
            "{id} must PASS for hermes (applicable to its declaration)"
        );
    }

    // Pause is BestEffort on every OS (CP-a) — APPLICABLE, demonstrated with
    // the qualifier causes, never skipped. Pin that it is not a skip.
    assert_eq!(
        report.section(section_ids::PAUSE),
        Some(&SectionResult::Pass),
        "BestEffort pause is applicable: the best-effort path must be demonstrated"
    );

    // The one declaration-justified metering skip: SelfReported never owes
    // EngineObserved proof — and the reason NAMES the declaration.
    assert_eq!(
        report.section(section_ids::METERING_ENGINE_OBSERVED),
        Some(&SectionResult::NotApplicable {
            reason: "the declaration declares self-reported metering, so the engine-observed \
                     section does not apply"
                .to_string()
        })
    );

    // The native-launch shape: the code-declared gateway argv is contract, so
    // the config section cannot observe delivered config through a `--dump`
    // seam — not_applicable with the justification, never a silent pass.
    match report.section(section_ids::CONFIG_MAPPING) {
        Some(SectionResult::NotApplicable { reason }) => {
            assert!(
                reason.contains("native") && reason.contains("--dump"),
                "the config skip must name the code-declared launch seam: {reason}"
            );
        }
        other => panic!("expected NotApplicable for config_mapping, got {other:?}"),
    }

    // The report carries the contract version the run was governed by
    // (retro #163, finding B8) — the frozen v1 this engine negotiates.
    assert_eq!(report.contract_version, "1.0.0");
}

/// The serialization lock for THIS binary's PATH-mutating tests. Plain
/// `cargo test` runs a binary's tests on parallel threads (nextest gives each
/// test its own process, where the lock is a no-op), so the two tests that
/// mutate the process-global `PATH`/`HERMES_SHIM_ARGS` must hold this lock for
/// the whole mutate→spawn→restore journey — the "exactly one env-mutating
/// test per binary" discipline, kept honest with a second such test.
static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Panic-safe restore of the process-global env mutation: takes the originals
/// at construction and restores them in `Drop`, so a panic anywhere between
/// mutate and teardown cannot leak `PATH`/`HERMES_SHIM_ARGS` into this
/// binary's other tests (review finding — the manual restore blocks only
/// covered the non-panicking path).
struct EnvRestore {
    path: Option<std::ffi::OsString>,
    shim_args: Option<std::ffi::OsString>,
}

impl EnvRestore {
    /// Capture the originals and install the shim dir at the front of `PATH`
    /// plus the scripted shim args. SAFETY: process-global mutation, performed
    /// under [`PATH_LOCK`], before any child spawn.
    unsafe fn install(shim_dir: &TempDir, shim_args: &str) -> Self {
        let original_path = std::env::var_os("PATH").map(|v| v.to_os_string());
        let original_shim_args = std::env::var_os("HERMES_SHIM_ARGS");
        let joined = {
            let mut paths: Vec<PathBuf> =
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
            paths.insert(0, shim_dir.path().to_path_buf());
            std::env::join_paths(paths).expect("join PATH")
        };
        unsafe {
            std::env::set_var("PATH", &joined);
            std::env::set_var("HERMES_SHIM_ARGS", shim_args);
        }
        Self {
            path: original_path,
            shim_args: original_shim_args,
        }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        // SAFETY: restoring the captured originals under the same PATH_LOCK
        // journey that installed them.
        unsafe {
            match self.path.take() {
                Some(original) => std::env::set_var("PATH", original),
                None => std::env::remove_var("PATH"),
            }
            match self.shim_args.take() {
                Some(original) => std::env::set_var("HERMES_SHIM_ARGS", original),
                None => std::env::remove_var("HERMES_SHIM_ARGS"),
            }
        }
    }
}

/// Stops the named instance on Drop — a subject started with a 10-minute
/// `--linger-ms` must never outlive a failed test (review finding: the
/// success-path-only `stop` left a running orphan on any mid-closure error).
struct StopOnDrop<'a, 'e> {
    facade: &'a hemaka_engine::Blocking<'e>,
    name: &'a str,
}

impl Drop for StopOnDrop<'_, '_> {
    fn drop(&mut self) {
        let _ = self
            .facade
            .stop(self.name, Some(std::time::Duration::from_secs(5)));
    }
}

/// Retro #163 (finding B11): the memory section's probe twin proves the
/// attach/deliver/detach MECHANISM, but on the probe's own declared env var —
/// the SUBJECT's own declared delivery (hermes: `memory.dir` → `HERMES_HOME`)
/// was never exercised by the kit. This test drives the hermes SUBJECT itself
/// through the kit's shim sandbox with a `--dump` seam scripted onto the shim,
/// attaches a filesystem backing, starts the subject, and proves the managed
/// Memory Backing dir reached the SUBJECT'S process as `HERMES_HOME` — the
/// exact delivery the contract claims for a declared mapping.
#[test]
fn hermes_subject_receives_the_managed_memory_dir_through_hermes_home() {
    let _env_guard = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // ---- Sandbox setup: the PATH shim; the shim script carries the dump
    // seam (plus linger so the started subject stays up while we poll). The
    // guard restores the env in Drop — panic-safe by construction.
    let shim_dir = TempDir::new().unwrap();
    let _shim = install_shim(&shim_dir);
    let state = TempDir::new().unwrap();
    let dump = state.path().join("hermes-subject-memory-dump.txt");
    let dump_str = dump.to_string_lossy().into_owned();
    let _env =
        unsafe { EnvRestore::install(&shim_dir, &format!("--dump {dump_str} --linger-ms 600000")) };

    // Drive the SUBJECT through the public engine API (the same surface the
    // harness itself drives).
    let subject_delivery = || -> Result<(), String> {
        let engine = Engine::open(Some(state.path().to_path_buf()))
            .map_err(|e| format!("engine open: {e}"))?;
        let facade = engine.blocking();
        facade
            .register_with_adapter("hermes", &AdapterRef::Native("hermes".to_string()))
            .map_err(|e| format!("register: {e}"))?;
        let managed = facade
            .attach_memory("hermes", MemoryBackingKind::Filesystem)
            .map_err(|e| format!("attach_memory: {e}"))?;
        // The subject's OWN declared delivery fact (DC-10): hermes maps the
        // reserved key, so `declared` must read true for the subject itself.
        let status = facade
            .memory_status("hermes")
            .map_err(|e| format!("memory_status: {e}"))?
            .ok_or("memory_status read None after attach")?;
        if !status.declared {
            return Err("the hermes subject's own `declared` fact must read true".to_string());
        }
        facade
            .start("hermes")
            .map_err(|e| format!("subject start: {e}"))?;
        // From here to the end of the closure the subject is RUNNING with a
        // 10-minute linger: the guard stops it on EVERY exit path, success or
        // error — a failed proof must not orphan the process.
        let _stop_guard = StopOnDrop {
            facade: &facade,
            name: "hermes",
        };
        // EXACT-value proof: the subject's process received precisely the
        // managed dir through HERMES_HOME (the `env=` dump-line shape).
        let needle = format!("env=HERMES_HOME={}", managed.display());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let hit = std::fs::read_to_string(&dump)
                .map(|text| text.lines().any(|line| line == needle))
                .unwrap_or(false);
            if hit {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "the hermes subject never received the managed memory dir through \
                     HERMES_HOME (expected `{needle}` in {})",
                    dump.display()
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        // Detach needs a TERMINAL state: stop explicitly first (the guard
        // below stays as the safety net for the error paths, and re-stopping
        // a stopped instance is a swallowed no-op).
        let _ = facade.stop("hermes", Some(std::time::Duration::from_secs(5)));
        facade
            .detach_memory("hermes")
            .map_err(|e| format!("detach_memory: {e}"))?;
        Ok(())
    };
    let outcome = subject_delivery();
    outcome.unwrap_or_else(|detail| panic!("hermes subject memory delivery: {detail}"));
}
