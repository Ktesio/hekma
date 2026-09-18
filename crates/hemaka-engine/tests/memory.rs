//! Integration tests for story 5-1: attach a managed `filesystem` Memory
//! Backing to an Agent Instance (spine AD-11), driven through the engine's
//! PUBLIC API + blocking facade only (spine AD-2 — if this file needs a
//! `pub(crate)` item, that is a signal the public surface is insufficient).
//!
//! What is proven here, end to end:
//!
//! * **AC1** — attach creates the managed directory INSIDE the Agent Home
//!   (path authority: the path comes FROM the engine, never computed here) and
//!   the public read reports the attachment.
//! * **AC2** — the SAME attach→start sequence works identically on the mock
//!   adapter and a manifest adapter, with the descriptor reaching each.
//! * **AC3** — attach/detach on a non-terminal instance are rejected with NO
//!   side effect (the guard is pure persisted-state validation, DC-3).
//! * **AC4** — the managed directory's contents survive stop/start cycles AND
//!   a real engine restart (`Engine::drop` + `Engine::open`, which runs orphan
//!   adoption) BYTE-identically — nested directories and non-UTF-8 bytes
//!   included (DC-7: survival by non-interference, not by copy logic).
//! * **DC-10** — delivery honesty: the public read distinguishes a mapping that
//!   targets the reserved key from one that does not, an unmapped start still
//!   SUCCEEDS, and the injected key never reaches `effective-config.json`
//!   (story 3-4's honest-provenance split).
//!
//! Determinism (DC-9): nothing here sleeps to await state. The only waiting is
//! a bounded poll on a COMMITTED artifact (the `fake_agent --dump` file), the
//! same pattern the secret-delivery proof uses.
//!
//! `fake_agent` staleness note (`EXISTENCE IS NOT FRESHNESS`): if a local run
//! fails oddly around the dump file, `rm -f target/debug/fake_agent*` and
//! rebuild before debugging — a stale helper silently does less.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hemaka_engine::{
    AdapterRef, ConfigLayer, Engine, LifecycleState, MemoryBackingKind, RegistryError, SourceLayer,
};
use tempfile::TempDir;

/// Open an engine over a fresh temp state dir (the `lifecycle.rs` shape).
fn open(base: &TempDir) -> Engine {
    Engine::open(Some(base.path().to_path_buf())).expect("open engine")
}

/// Write a `fake_agent` manifest whose `[lifecycle.start]` exec points at the
/// helper binary with `args`, optionally appending a `[config.*]` section body.
fn write_fake_manifest(dir: &Path, kind: &str, args: &[&str], config_section: Option<&str>) {
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
{config_section}"#,
        exec = bin.to_string_lossy(),
        config_section = config_section.unwrap_or(""),
    );
    std::fs::write(dir.join("adapter.toml"), body).unwrap();
}

/// A manifest that dumps its received argv + env at startup and DECLARES a
/// mapping for the reserved memory key (env `AGENT_MEMORY_DIR` — deliberately
/// NOT a copy of any engine/mock constant, proving the DECLARED target is what
/// carries the value).
const MEMORY_MAPPED_CONFIG: &str = r#"
[config."memory.dir"]
env = "AGENT_MEMORY_DIR"
"#;

const MEMORY_ENV_VAR: &str = "AGENT_MEMORY_DIR";

/// Poll until the dump file exists and contains an `env=` line for `var`
/// (bounded; polls a COMMITTED artifact, never sleeps to await state).
fn poll_dump_for(dump: &Path, var: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(dump) {
            if text.contains(&format!("env={var}=")) {
                return text;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the agent never wrote its dump at {}: expected env={var}=…",
            dump.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Recursively collect `(relative path, bytes)` for every file under `dir`,
/// sorted by path — the byte-identical comparison vehicle (AC4).
fn snapshot_tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let rel = if prefix.is_empty() {
                entry.file_name().to_string_lossy().into_owned()
            } else {
                format!("{prefix}/{}", entry.file_name().to_string_lossy())
            };
            if path.is_dir() {
                walk(&path, &rel, out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, "", &mut out);
    out
}

// ---- AC1: attach creates the managed directory under path authority ----

#[test]
fn attaching_a_filesystem_backing_creates_the_managed_directory_inside_the_agent_home() {
    // AC1 + DC-1: the directory exists at the ENGINE-computed path inside the
    // Agent Home, and the public read reports the kind + the same path. This
    // test never joins "memory" itself — the path authority stays in the engine.
    let state = TempDir::new().unwrap();
    let engine = open(&state);
    let facade = engine.blocking();

    let registered = facade.register("demo", "mock").unwrap();
    let home = PathBuf::from(&registered.agent_home);
    assert!(!home.join("nothing-yet").exists());

    let dir = facade
        .attach_memory("demo", MemoryBackingKind::Filesystem)
        .unwrap();

    // The returned path IS the managed directory, and it lives INSIDE the home.
    assert_eq!(dir.parent(), Some(home.as_path()));
    assert!(dir.is_dir(), "attach must create the managed directory");

    // The public read reports the attachment (kind + path + the DC-10 fact).
    let status = facade.memory_status("demo").unwrap().expect("attached");
    assert_eq!(status.kind, MemoryBackingKind::Filesystem);
    assert_eq!(status.dir, dir);
    // The builtin mock declares a mapping for the reserved key, so delivery IS
    // declared for this adapter.
    assert!(status.declared, "mock maps the reserved key to an env var");

    // Idempotence (A-6): re-attaching the same kind succeeds and keeps the path.
    let again = facade
        .attach_memory("demo", MemoryBackingKind::Filesystem)
        .unwrap();
    assert_eq!(again, dir);

    // Nothing attached ⇒ `None`; unknown instance ⇒ NotFound (never `None`).
    facade.register("bare", "mock").unwrap();
    assert!(
        facade.memory_status("bare").unwrap().is_none(),
        "registered-but-unattached reads as None"
    );
    let err = facade.memory_status("ghost").unwrap_err();
    assert!(matches!(err, RegistryError::NotFound { ref name } if name == "ghost"));
}

// ---- AC3: no hot-swap, and a guard rejection mutates NOTHING ----

#[test]
fn attach_and_detach_on_a_running_instance_are_rejected_with_no_side_effect() {
    // AC3 + DC-3: genuinely Running (a live fake_agent, not a seeded row — the
    // full non-terminal matrix is pinned deterministically in registry.rs's
    // unit tests against seeded states). A rejection leaves NO side effect:
    // no directory, no persisted row (a guard that rejects AFTER mutating is
    // the bug worth catching).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(manifest.path(), "svc", &["--linger-ms", "600000"], None);

    let engine = open(&state);
    let facade = engine.blocking();
    let registered = facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();

    let started = facade.start("svc").unwrap();
    assert_eq!(started.state, LifecycleState::Running);

    let err = facade
        .attach_memory("svc", MemoryBackingKind::Filesystem)
        .unwrap_err();
    match &err {
        RegistryError::MemoryBackingHotSwap { name, state } => {
            assert_eq!(name, "svc");
            assert_eq!(state, "running");
        }
        other => panic!("expected MemoryBackingHotSwap, got {other:?}"),
    }

    let detach_err = facade.detach_memory("svc").unwrap_err();
    assert!(matches!(
        detach_err,
        RegistryError::MemoryBackingHotSwap { .. }
    ));

    // NO side effect from either rejected op: no persisted row, and NO memory
    // directory inside the Agent Home (the layout leaf is pinned here because
    // ENGINE tests own the layout contract; `kt` may never name it).
    assert!(facade.memory_status("svc").unwrap().is_none());
    let home_entries: Vec<String> = std::fs::read_dir(&registered.agent_home)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !home_entries.iter().any(|n| n == "memory"),
        "guard must reject BEFORE creating any directory: {home_entries:?}"
    );

    // Stop → terminal again → attach now succeeds (the guard tracks the
    // PERSISTED state, and the transition restored a terminal one).
    let stopped = facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
    assert_eq!(stopped.state, LifecycleState::Stopped);
    let dir = facade
        .attach_memory("svc", MemoryBackingKind::Filesystem)
        .unwrap();
    assert!(dir.is_dir());
}

// ---- AC4: byte-identical survival across stop/start AND an engine restart ----

#[test]
fn managed_memory_contents_survive_stop_start_and_an_engine_restart_byte_identically() {
    // THE headline test. Attach → start → write a known byte payload (nested
    // subdirectory + a NON-UTF-8 byte, so "byte-identical" is real) → stop →
    // start → DROP the engine and `Engine::open` the same state dir (the real
    // restart path, including orphan adoption) → assert every byte equal.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(manifest.path(), "svc", &["--linger-ms", "600000"], None);

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let dir = facade
        .attach_memory("svc", MemoryBackingKind::Filesystem)
        .unwrap();

    facade.start("svc").unwrap();

    // The payload: plain text, a nested subdirectory, and a non-UTF-8 byte.
    std::fs::write(dir.join("notes.txt"), b"operator data v1").unwrap();
    std::fs::create_dir_all(dir.join("nested/deeper")).unwrap();
    std::fs::write(dir.join("nested/deeper/blob.bin"), [0xFF, 0x00, 0xFE, 0x42]).unwrap();
    let before = snapshot_tree(&dir);
    assert_eq!(before.len(), 2, "payload: text + nested binary (non-UTF-8)");

    let stopped = facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
    assert_eq!(stopped.state, LifecycleState::Stopped);
    assert_eq!(
        snapshot_tree(&dir),
        before,
        "stop must not touch the managed contents"
    );

    // Start again (a fresh cycle over surviving contents).
    facade.start("svc").unwrap();
    assert_eq!(snapshot_tree(&dir), before, "start must not touch contents");
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();

    // THE ENGINE RESTART half: drop the Engine entirely and reopen the same
    // state dir. Engine::open runs orphan adoption — the exact path that must
    // not disturb memory/.
    drop(engine);
    let reopened = open(&state);
    let facade = reopened.blocking();
    assert_eq!(
        snapshot_tree(&dir),
        before,
        "an engine restart must leave every byte in place"
    );
    // The ATTACHMENT survives the restart too (persisted in SQLite, DC-2).
    let status = facade.memory_status("svc").unwrap().expect("persisted");
    assert_eq!(status.kind, MemoryBackingKind::Filesystem);
    assert_eq!(status.dir, dir);

    // And the restarted engine starts the instance cleanly alongside them.
    facade.start("svc").unwrap();
    assert_eq!(snapshot_tree(&dir), before);
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
}

#[test]
fn a_detached_instance_starts_cleanly_with_the_directory_still_present() {
    // Untested-mode-combination guard (AI-64 #4): attach → start → stop →
    // detach → start. Detach is METADATA ONLY (A-4): the directory and its
    // contents remain, and the detached instance starts cleanly alongside them.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(manifest.path(), "svc", &["--linger-ms", "600000"], None);

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let dir = facade
        .attach_memory("svc", MemoryBackingKind::Filesystem)
        .unwrap();
    std::fs::write(dir.join("keep.txt"), b"survives a detach").unwrap();

    facade.start("svc").unwrap();
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();

    facade.detach_memory("svc").unwrap();
    assert!(
        facade.memory_status("svc").unwrap().is_none(),
        "row cleared"
    );
    assert_eq!(
        std::fs::read(dir.join("keep.txt")).unwrap(),
        b"survives a detach",
        "detach must never delete operator data"
    );

    // Detached (but still terminal): starts cleanly with the directory present.
    facade.start("svc").unwrap();
    assert_eq!(
        std::fs::read(dir.join("keep.txt")).unwrap(),
        b"survives a detach",
        "a post-detach start must not disturb the leftover directory"
    );
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();

    // Re-attaching later re-adopts the existing contents (same directory).
    let again = facade
        .attach_memory("svc", MemoryBackingKind::Filesystem)
        .unwrap();
    assert_eq!(again, dir);
    assert!(dir.join("keep.txt").is_file());
}

// ---- AC2 + DC-10: descriptor delivery, parity, and honesty ----

#[test]
fn the_same_attach_sequence_works_on_the_mock_and_a_manifest_adapter() {
    // AC2, table-driven so the test literally IS "the same command sequence":
    // register → attach filesystem → observe the descriptor delivered at the
    // reserved key. Two legs:
    //
    // * **native `mock`** — INERT by recorded decision (NativeHasNoLaunch; a
    //   launchable native agent does not exist this epic), so per the shipped
    //   inert-mock strategy (supervisor.rs's Decision-8 proof) the observation
    //   vehicle is the MAPPED LAUNCH the start seam itself produces: resolve the
    //   mock's code-declared mapping + apply the invocation override exactly as
    //   `start_inner` does, and assert the declared env target carries the path.
    // * **manifest adapter** — a REAL child (`fake_agent --dump`) whose declared
    //   `[config."memory.dir"] env` receives the path, observed in the dump.
    struct Leg {
        label: &'static str,
        manifest_config: Option<String>,
        dump: Option<PathBuf>,
    }

    let tmp = TempDir::new().unwrap();
    let legs = [
        Leg {
            label: "mock (native builtin)",
            manifest_config: None,
            dump: None,
        },
        Leg {
            label: "manifest adapter",
            manifest_config: Some(MEMORY_MAPPED_CONFIG.to_string()),
            dump: Some(tmp.path().join("manifest-leg.dump")),
        },
    ];

    for leg in legs {
        let state = TempDir::new().unwrap();
        let manifest_dir = TempDir::new().unwrap();
        let mut args = vec!["--linger-ms", "600000"];
        let dump_arg: String;
        if let Some(dump) = &leg.dump {
            dump_arg = format!("{}", dump.display());
            args.push("--dump");
            args.push(&dump_arg);
        }
        write_fake_manifest(
            manifest_dir.path(),
            "parity",
            &args,
            leg.manifest_config.as_deref(),
        );

        let engine = open(&state);
        let facade = engine.blocking();

        // THE SAME COMMAND SEQUENCE both legs run:
        let registered = if leg.manifest_config.is_some() {
            facade
                .register_with_adapter(
                    "svc",
                    &AdapterRef::Manifest(manifest_dir.path().to_path_buf()),
                )
                .unwrap()
        } else {
            facade.register("svc", "mock").unwrap()
        };
        let dir = facade
            .attach_memory("svc", MemoryBackingKind::Filesystem)
            .unwrap();
        assert_eq!(
            dir.parent(),
            Some(Path::new(&registered.agent_home)),
            "{}: the managed directory lives inside the Agent Home",
            leg.label
        );
        // The public read reports DELIVERY DECLARED for both adapters (the mock
        // code-declares it; the fixture manifest declares it).
        let status = facade.memory_status("svc").unwrap().expect("attached");
        assert!(
            status.declared,
            "{}: the mapping must target the reserved key",
            leg.label
        );

        if let Some(dump) = &leg.dump {
            // Manifest leg: START for real and observe the child's environment.
            facade.start("svc").unwrap();
            let dump_text = poll_dump_for(dump, MEMORY_ENV_VAR);
            let expected = format!("env={MEMORY_ENV_VAR}={}", dir.display());
            assert!(
                dump_text.contains(&expected),
                "{}: the engine-computed path must reach the child's declared env; \
                 want {expected:?} in dump:\n{dump_text}",
                leg.label
            );
            // Honest provenance (the CORRECTION): the injected value is a
            // delivery mechanism, NOT operator configuration — it must NOT be in
            // the effective-config snapshot.
            let home = PathBuf::from(&registered.agent_home);
            let snapshot = std::fs::read_to_string(home.join("effective-config.json"))
                .expect("snapshot written at start");
            assert!(
                !snapshot.contains("memory.dir") && !snapshot.contains(dir.to_str().unwrap()),
                "{}: the reserved key must stay out of effective-config.json:\n{snapshot}",
                leg.label
            );
            facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
        } else {
            // Mock leg (inert): reproduce the start seam's EXACT transform via
            // the public API — the invocation override carrying the managed dir
            // folded into `effective_config`, the mock's code-declared mapping
            // resolved, and the mapping APPLIED onto a bare launch (what a
            // launchable native adapter would receive).
            let overrides = ConfigLayer::parse(
                SourceLayer::InvocationOverride,
                "<memory-dir invocation override>",
                &format!("[memory]\ndir = '{}'\n", dir.display()),
            )
            .expect("override layer parses (memory.dir is a KNOWN key)");
            let effective = facade.effective_config("svc", overrides).unwrap();
            let mapping = hemaka_engine::adapter::resolve_config_mapping("mock", None).unwrap();
            // The declared target's NAME comes from the mapping itself — no
            // engine-internal constant is named here.
            let env_var = mapping
                .target("memory.dir")
                .and_then(|t| t.env_var())
                .expect("the builtin mock declares an env target for memory.dir")
                .to_string();
            let mut launch = hemaka_engine::adapter::StartLaunch {
                exec: "mock".to_string(),
                args: Vec::new(),
                env: BTreeMap::new(),
            };
            hemaka_engine::adapter::apply_config_mapping(
                &mut launch,
                &mapping,
                &effective,
                &BTreeMap::new(),
                Path::new(&registered.agent_home),
            )
            .unwrap();
            assert_eq!(
                launch.env.get(&env_var),
                Some(&dir.to_string_lossy().into_owned()),
                "{}: the managed path lands in the mock's declared env target ({env_var})",
                leg.label
            );
        }
    }
}

#[test]
fn an_attached_but_unmapped_manifest_start_succeeds_and_reports_undelivered() {
    // DC-10, the silent-failure case: the adapter declares NO mapping for the
    // reserved key. The start still SUCCEEDS (refusing an otherwise-healthy
    // agent would regress the guarantee floor), the public read reports the
    // backing UNDELIVERED, and the stderr notice fires (asserted end-to-end at
    // the CLI level, which captures the engine process's stderr).
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(
        manifest.path(),
        "unmapped",
        &["--linger-ms", "600000"],
        None,
    );

    let engine = open(&state);
    let facade = engine.blocking();
    let registered = facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let dir = facade
        .attach_memory("svc", MemoryBackingKind::Filesystem)
        .unwrap();
    let status = facade.memory_status("svc").unwrap().expect("attached");
    assert_eq!(status.kind, MemoryBackingKind::Filesystem);
    assert_eq!(status.dir, dir);
    assert!(
        !status.declared,
        "no declared target for the reserved key ⇒ undelivered"
    );

    // The directory guarantee holds regardless: start succeeds.
    let started = facade.start("svc").unwrap();
    assert_eq!(started.state, LifecycleState::Running);
    assert!(dir.is_dir());

    // …and the snapshot STILL carries neither the key nor the path.
    let home = PathBuf::from(&registered.agent_home);
    let snapshot = std::fs::read_to_string(home.join("effective-config.json")).unwrap();
    assert!(
        !snapshot.contains("memory.dir") && !snapshot.contains(dir.to_str().unwrap()),
        "the reserved key must stay out of effective-config.json even unmapped:\n{snapshot}"
    );
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
}

// ---- Review hardening: self-heal, symlink refusal, native suppression, spoof ----

#[test]
fn a_hand_deleted_directory_is_self_healed_at_start_and_re_attach() {
    // The SELF-HEAL contract (paths.rs layout docs + the start-path defensive
    // create): a manual delete of <home>/memory while stopped must not wedge
    // future starts (start recreates it) nor future same-kind re-attaches
    // (attach recreates it). Without these branches an operator cleanup would
    // silently void the directory guarantee — the adapter's declared target
    // would receive a path to a nonexistent directory.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(manifest.path(), "svc", &["--linger-ms", "600000"], None);

    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let dir = facade
        .attach_memory("svc", MemoryBackingKind::Filesystem)
        .unwrap();

    // Start once (proves the normal path), stop, then delete out of band.
    facade.start("svc").unwrap();
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(!dir.exists(), "setup: the hand-delete took effect");

    // START self-heals: the instance starts cleanly and the directory is back.
    facade.start("svc").unwrap();
    assert!(
        dir.is_dir(),
        "start must recreate a hand-deleted memory dir"
    );
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();

    // RE-ATTACH self-heals too: same kind after another hand-delete succeeds
    // idempotently and recreates it (A-6).
    std::fs::remove_dir_all(&dir).unwrap();
    let again = facade
        .attach_memory("svc", MemoryBackingKind::Filesystem)
        .unwrap();
    assert_eq!(again, dir);
    assert!(dir.is_dir(), "re-attach must recreate a hand-deleted dir");
}

#[test]
#[cfg(unix)]
fn a_symlinked_memory_path_is_refused_not_followed() {
    // Containment (unix-gated HONESTLY, per the AI-35 disclosure convention:
    // this test does not exist on Windows rather than passing vacuously there —
    // portable directory-link creation would need a new crate, which this story
    // forbids): if <home>/memory exists as a SYMLINK, creation must refuse it
    // rather than follow it — otherwise managed writes (and the delivered path)
    // escape the Agent Home. Both public surfaces that materialize the
    // directory are exercised: the attach/re-attach path and the start-time
    // self-heal.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // A LAUNCHABLE adapter (the inert mock rejects start before the memory
    // block is ever reached); the child never has to run — the refusal fires
    // in the pre-transition block.
    write_fake_manifest(manifest.path(), "demo", &["--linger-ms", "600000"], None);
    let engine = open(&state);
    let facade = engine.blocking();
    facade
        .register_with_adapter("demo", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let outside = state.path().join("outside-memory");
    std::fs::create_dir_all(&outside).unwrap();

    // Attach once for real, then swap the managed path for a symlink.
    let dir = facade
        .attach_memory("demo", MemoryBackingKind::Filesystem)
        .unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&outside, &dir).unwrap();
    let before = std::fs::read_dir(&outside).unwrap().count();

    // RE-ATTACH must refuse (and not write through the link).
    let err = facade
        .attach_memory("demo", MemoryBackingKind::Filesystem)
        .unwrap_err();
    assert!(
        err.to_string().contains("symlink"),
        "re-attach must refuse a symlinked managed path: {err}"
    );
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        before,
        "nothing may be written through the link"
    );

    // START's defensive self-heal must refuse too (same planted link).
    let err = facade.start("demo").unwrap_err();
    assert!(
        err.to_string().contains("symlink"),
        "start must refuse a symlinked managed path: {err}"
    );
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        before,
        "still nothing written through the link"
    );

    // Removing the link restores the normal world (sanity tail).
    std::fs::remove_file(&dir).unwrap();
    facade
        .attach_memory("demo", MemoryBackingKind::Filesystem)
        .unwrap();
    assert!(dir.is_dir());
}

#[test]
fn a_native_backing_never_injects_the_reserved_key_or_creates_the_directory_at_start() {
    // V2 regression pin for the filesystem-only gate: `native` is the 5-2
    // delegation marker — starting with one attached must NOT create
    // <home>/memory, must NOT inject the reserved key (even when the adapter
    // DECLARES a mapping for it), and must stay silent about delivery. If the
    // `.filter(kind == Filesystem)` gate ever regressed, this exact setup would
    // start injecting and this test fails on the env line below.
    let state = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    // The dump target lives OUTSIDE the Agent Home so the home stays pristine
    // for the directory-absence assertion below.
    let dump = state.path().join("native-suppression.dump");
    let dump_arg = format!("{}", dump.display());
    // Write the FINAL manifest BEFORE registering — registration snapshots the
    // launch (exec + args), so a later rewrite would not reach the child.
    write_fake_manifest(
        manifest.path(),
        "nat",
        &["--linger-ms", "600000", "--dump", &dump_arg],
        Some(MEMORY_MAPPED_CONFIG),
    );

    let engine = open(&state);
    let facade = engine.blocking();
    let registered = facade
        .register_with_adapter("nat", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let home = PathBuf::from(&registered.agent_home);

    // Attach NATIVE (public API; vocabulary shipped in 5-1, behavior is 5-2's)
    // and confirm no directory was created.
    let dir = facade
        .attach_memory("nat", MemoryBackingKind::Native)
        .unwrap();
    assert!(!dir.exists(), "native attaches no directory");

    // Start the REAL child; wait for its dump to exist (any content proves the
    // agent ran — we cannot poll for the env line because its ABSENCE is the
    // assertion).
    facade.start("nat").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match std::fs::read_to_string(&dump) {
            Ok(text) if text.contains("arg=") => break,
            _ => {
                assert!(
                    Instant::now() < deadline,
                    "the agent never wrote its dump at {}",
                    dump.display()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    let dump_text = std::fs::read_to_string(&dump).unwrap();
    assert!(
        !dump_text.contains("env=AGENT_MEMORY_DIR="),
        "a native backing must not inject the reserved key even when mapped:\n{dump_text}"
    );
    assert!(
        !home.join("memory").exists(),
        "a native backing must not create the managed directory at start"
    );
    facade.stop("nat", Some(Duration::from_secs(5))).unwrap();
}

// ---- Story 5-2, AC2: portability — a copied Agent Home serves memory intact ----

#[test]
fn a_copied_agent_home_serves_a_byte_identical_memory_tree_and_reports_the_backing() {
    // THE portability proof (AC2, DC-5): the documented copy procedure is
    // "stop first, copy the whole state dir, same relative layout" — and it
    // must Just Work because an Agent Home is a plain tree relative to
    // `state_base` and the backing row rides inside state.db. No copy/sync
    // feature code exists; this test IS the proof that none is needed.
    //
    // Both kinds are covered: the filesystem home carries its memory/ contents
    // BYTE-identically; the native home carries only the delegation row (and
    // still creates no directory on machine B).
    let state_a = TempDir::new().unwrap();
    let manifest = TempDir::new().unwrap();
    write_fake_manifest(manifest.path(), "svc", &["--linger-ms", "600000"], None);

    // Machine A: register + attach BOTH kinds' instances and populate memory.
    let engine = open(&state_a);
    let facade = engine.blocking();
    facade
        .register_with_adapter("svc", &AdapterRef::Manifest(manifest.path().to_path_buf()))
        .unwrap();
    let fs_dir = facade
        .attach_memory("svc", MemoryBackingKind::Filesystem)
        .unwrap();
    facade.start("svc").unwrap();
    std::fs::write(fs_dir.join("notes.txt"), b"operator data v1").unwrap();
    std::fs::create_dir_all(fs_dir.join("nested/deeper")).unwrap();
    std::fs::write(
        fs_dir.join("nested/deeper/blob.bin"),
        [0xFF, 0x00, 0xFE, 0x42],
    )
    .unwrap();
    let payload = snapshot_tree(&fs_dir);
    assert_eq!(payload.len(), 2);
    facade.stop("svc", Some(Duration::from_secs(5))).unwrap();

    facade.register("delegated", "mock").unwrap();
    facade
        .attach_memory("delegated", MemoryBackingKind::Native)
        .unwrap();

    // THE COPY: stop-first already held (both instances terminal); the whole
    // state dir moves to a second root with its relative layout preserved.
    let state_b = TempDir::new().unwrap();
    for entry in std::fs::read_dir(state_a.path()).unwrap().flatten() {
        let target = state_b.path().join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir_recursive(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }

    // Drop A BEFORE opening B so two engines never hold one SQLite file.
    drop(engine);

    // Machine B: open the copied state and verify everything traveled.
    let engine_b = open(&state_b);
    let facade_b = engine_b.blocking();

    // The attachment rows survived inside state.db (DC-2), with their typed
    // guarantee levels (story 5-2).
    let fs_status = facade_b.memory_status("svc").unwrap().expect("row travels");
    assert_eq!(fs_status.kind, MemoryBackingKind::Filesystem);
    assert_eq!(
        fs_status.guarantee,
        hemaka_engine::GuaranteeLevel::ManagedDirByteDurable
    );
    let native_status = facade_b
        .memory_status("delegated")
        .unwrap()
        .expect("row travels");
    assert_eq!(native_status.kind, MemoryBackingKind::Native);
    assert_eq!(
        native_status.guarantee,
        hemaka_engine::GuaranteeLevel::HomePersistenceOnly
    );
    assert!(
        !native_status.dir.exists(),
        "machine B must not materialize a directory for a delegation marker"
    );

    // The filesystem memory tree is BYTE-identical at the recomputed path.
    let fs_dir_b = fs_status.dir;
    assert_eq!(
        snapshot_tree(&fs_dir_b),
        payload,
        "the copied home serves the exact bytes"
    );

    // And machine B runs the instance cleanly against the traveled memory.
    let started = facade_b.start("svc").unwrap();
    assert_eq!(started.state, LifecycleState::Running);
    assert_eq!(
        snapshot_tree(&fs_dir_b),
        payload,
        "start leaves bytes alone"
    );
    facade_b.stop("svc", Some(Duration::from_secs(5))).unwrap();
}

/// Copy a directory tree preserving relative structure (test-local helper for
/// the portability proof; deliberately NOT shared machinery — DC-5 ships no
/// copy feature).
fn copy_dir_recursive(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap().flatten() {
        let target = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir_recursive(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}
