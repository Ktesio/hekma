/// Test-only mutual exclusion for the process-global state-dir env vars
/// (`HEKMA_STATE_DIR` / `KTESIO_STATE_DIR`). Save/restore around each test
/// does NOT prevent two env-mutating tests interleaving under the default
/// `--test-threads`: their mutation windows overlap, and a legacy-name test
/// racing an alias-name test makes BOTH see `PathError::ConflictingStateDir`
/// (observed as a workspace-run flake 2026-10-10, green on rerun). Every
/// test that sets or clears either name holds this lock for its whole
/// mutation span, mirroring the `STATE_DIR_ENV_LOCK` discipline in
/// `crates/hekma`'s CLI tests. A poisoned lock is fine — the guard is only
/// for env mutual exclusion, so `into_inner` recovers it.
pub(crate) static STATE_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use super::*;
use tempfile::TempDir;

fn name(s: &str) -> InstanceName {
    InstanceName::new(s).unwrap()
}

#[test]
fn override_base_is_used_verbatim() {
    let tmp = TempDir::new().unwrap();
    let paths = EnginePaths::new(Some(tmp.path().to_path_buf())).unwrap();
    assert_eq!(paths.state_base(), tmp.path());
    assert_eq!(paths.state_db(), tmp.path().join("state.db"));
    assert_eq!(paths.agents_dir(), tmp.path().join("agents"));
    // Story 2-4: the engine secrets file is a state-dir-level file beside the
    // state DB (NOT per-Agent-Home), named secrets.toml.
    assert_eq!(paths.secrets_file(), tmp.path().join("secrets.toml"));
}

#[test]
fn two_names_get_disjoint_homes() {
    let tmp = TempDir::new().unwrap();
    let paths = EnginePaths::new(Some(tmp.path().to_path_buf())).unwrap();
    let a = paths.agent_home(&name("alpha"));
    let b = paths.agent_home(&name("beta"));
    assert_ne!(a, b);
    assert!(a.ends_with("agents/alpha"));
    assert!(b.ends_with("agents/beta"));
    // Config file lives inside the home.
    assert_eq!(paths.instance_config(&name("alpha")), a.join("config.toml"));
    // Story 2-3: the effective-config snapshot also lives inside the home,
    // as effective-config.json, distinct from the editable config.toml.
    assert_eq!(
        paths.effective_config_snapshot(&name("alpha")),
        a.join("effective-config.json")
    );
    // Story 5-1: the managed Memory Backing directory lives inside the home
    // as memory/ (path authority — one const, one accessor).
    assert_eq!(paths.agent_memory_dir(&name("alpha")), a.join("memory"));
}

#[test]
fn env_override_is_honored_when_no_explicit_base() {
    // Hold the shared env lock: this test mutates the process-global
    // KTESIO_STATE_DIR, which the alias-family tests (and the registry
    // path-failure test) also touch — save/restore alone does not stop
    // the mutation windows interleaving under default --test-threads.
    let _env_guard = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let prev = std::env::var_os(STATE_DIR_ENV);
    std::env::set_var(STATE_DIR_ENV, tmp.path());
    let paths = EnginePaths::new(None).unwrap();
    assert_eq!(paths.state_base(), tmp.path());
    match prev {
        Some(v) => std::env::set_var(STATE_DIR_ENV, v),
        None => std::env::remove_var(STATE_DIR_ENV),
    }
}

#[test]
fn relative_env_base_is_rejected() {
    // F7: a relative KTESIO_STATE_DIR must be refused (it would resolve
    // CWD-relative and leak a non-portable path). Save/restore the shared
    // env var like the sibling test, under the shared env lock (the
    // mutation window must not interleave with the alias-family tests).
    let _env_guard = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var_os(STATE_DIR_ENV);
    std::env::set_var(STATE_DIR_ENV, "relative/state/dir");
    let err = EnginePaths::new(None).unwrap_err();
    match prev {
        Some(v) => std::env::set_var(STATE_DIR_ENV, v),
        None => std::env::remove_var(STATE_DIR_ENV),
    }
    assert!(
        matches!(&err, PathError::RelativeStateDir { value } if value == "relative/state/dir"),
        "got {err:?}"
    );
}

#[test]
fn explicit_relative_override_is_trusted() {
    // The explicit Some(base) override is trusted verbatim even if relative
    // (embedding/tests own it); only the env-provided base is rejected.
    let paths = EnginePaths::new(Some(PathBuf::from("relative/base"))).unwrap();
    assert_eq!(paths.state_base(), Path::new("relative/base"));
}

// ---- v0.8.0 Hekma rename: the HEKMA_STATE_DIR alias ----

/// Save/restore BOTH state-dir env names around an env-mutating test
/// (the shared process env is racy across parallel tests; mirror the
/// existing save/restore discipline in the sibling tests above). The
/// whole set→run→restore span holds [`STATE_DIR_ENV_LOCK`] so no two
/// env-mutating tests in this test binary ever interleave.
fn with_state_dir_env<F>(alias: Option<&Path>, legacy: Option<&Path>, f: F)
where
    F: FnOnce(),
{
    let _env_guard = STATE_DIR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev_alias = std::env::var_os(STATE_DIR_ENV_ALIAS);
    let prev_legacy = std::env::var_os(STATE_DIR_ENV);
    let set = |name: &str, value: Option<&Path>| match value {
        Some(path) => std::env::set_var(name, path),
        None => std::env::remove_var(name),
    };
    set(STATE_DIR_ENV_ALIAS, alias);
    set(STATE_DIR_ENV, legacy);
    f();
    match prev_alias {
        Some(v) => std::env::set_var(STATE_DIR_ENV_ALIAS, v),
        None => std::env::remove_var(STATE_DIR_ENV_ALIAS),
    }
    match prev_legacy {
        Some(v) => std::env::set_var(STATE_DIR_ENV, v),
        None => std::env::remove_var(STATE_DIR_ENV),
    }
}

#[test]
fn alias_env_override_is_honored_when_no_explicit_base() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().to_path_buf();
    with_state_dir_env(Some(&base), None, || {
        let paths = EnginePaths::new(None).unwrap();
        assert_eq!(paths.state_base(), base);
    });
}

#[test]
fn alias_and_legacy_env_naming_the_same_dir_are_accepted() {
    // Both spellings of one path: NOT a conflict — scripts mid-migration
    // export both, and the state root is unambiguous.
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().to_path_buf();
    with_state_dir_env(Some(&base), Some(&base), || {
        let paths = EnginePaths::new(None).unwrap();
        assert_eq!(paths.state_base(), base);
    });
}

#[test]
fn alias_and_legacy_env_naming_different_dirs_are_rejected() {
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    with_state_dir_env(Some(a.path()), Some(b.path()), || {
        let err = EnginePaths::new(None).unwrap_err();
        assert!(
            matches!(&err, PathError::ConflictingStateDir { alias, legacy }
                if alias == a.path() && legacy == b.path()),
            "got {err:?}"
        );
    });
}

#[test]
fn legacy_env_override_alone_still_works() {
    // The legacy name keeps working unchanged (existing installs).
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().to_path_buf();
    with_state_dir_env(None, Some(&base), || {
        let paths = EnginePaths::new(None).unwrap();
        assert_eq!(paths.state_base(), base);
    });
}

#[test]
fn relative_alias_env_base_is_rejected() {
    with_state_dir_env(Some(Path::new("relative/alias")), None, || {
        let err = EnginePaths::new(None).unwrap_err();
        assert!(
            matches!(&err, PathError::RelativeStateDir { value } if value == "relative/alias"),
            "got {err:?}"
        );
    });
}

// ---- Story 11-2 (AI-24/AI-28): the shared atomic write helper ----

/// Every directory entry under `dir` whose name contains the temp marker.
fn temp_residue(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp-"))
        .collect()
}

#[test]
fn write_atomically_lands_the_bytes_and_leaves_no_temp_residue() {
    // The success path (the ONLY path a production config write should
    // ever take): the target holds the new bytes AND the temp is gone —
    // it was renamed onto the target, so it cannot litter the directory.
    // An overwrite (the common re-set path) behaves identically.
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("config.toml");

    write_atomically(&target, b"first bytes").unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"first bytes");
    assert!(temp_residue(tmp.path()).is_empty(), "no residue on create");

    write_atomically(&target, b"second bytes").unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"second bytes");
    assert!(
        temp_residue(tmp.path()).is_empty(),
        "no residue on overwrite"
    );
}

#[test]
fn write_atomically_rename_failure_leaves_the_target_and_no_temp() {
    // Injected rename failure: a DIRECTORY occupies the target path, so the
    // temp write succeeds but the rename cannot replace it. The error
    // surfaces, the target is untouched, and — the AI-24 residue half — the
    // helper's temp is cleaned up: a failed atomic write must not leave
    // `.tmp-*` litter in the Agent Home on any path.
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("config.toml");
    std::fs::create_dir(&target).unwrap();

    let err = write_atomically(&target, b"new bytes").unwrap_err();
    assert!(!err.to_string().is_empty(), "the OS detail is preserved");
    assert!(target.is_dir(), "the target is unchanged");
    assert!(
        temp_residue(tmp.path()).is_empty(),
        "a failed rename must leave NO temp residue; found {residue:?}",
        residue = temp_residue(tmp.path())
    );
}

#[test]
fn write_atomically_temp_write_failure_leaves_the_target_untouched() {
    // Injected temp-write failure: a DIRECTORY occupies a chosen temp path,
    // driven through `write_atomic_via` (the composition core) so the test
    // pins the temp path deterministically instead of racing the live
    // counter. The temp write itself fails, so: the error surfaces, the
    // target is never touched, and the helper created nothing — the planted
    // blocker is the only `tmp-`-marked entry (the helper neither added nor
    // removed anything).
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("config.toml");
    let planted = tmp.path().join("config.toml.tmp-pinned");
    std::fs::create_dir(&planted).unwrap();

    let err = write_atomic_via(&planted, &target, b"new bytes").unwrap_err();
    assert!(!err.to_string().is_empty());
    assert!(
        !target.exists(),
        "a failed temp write never touches the target"
    );
    // Only the planted directory remains — no helper-created file.
    let residue = temp_residue(tmp.path());
    assert_eq!(
        residue,
        vec![planted.file_name().unwrap().to_string_lossy()]
    );
}

#[test]
fn write_atomic_via_refuses_a_target_that_names_no_file() {
    // 2026-09-28 coverage batch: the guard arm — a target whose path has
    // NO file-name component (a filesystem root) is an InvalidInput
    // refusal naming the path, never a mysterious rename failure
    // downstream. The guard fires before any I/O, so nothing is touched.
    let err =
        write_atomic_via(&std::env::temp_dir(), std::path::Path::new("/"), b"bytes").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(err.to_string().contains("names no file"), "{err}");
}

#[test]
fn write_atomically_concurrent_same_target_writes_are_collision_safe() {
    // Review-1 patch 1: two THREADS overwriting the SAME target in one
    // process — the pid + thread-id + counter temp names never collide, so
    // both writers succeed and the published bytes are exactly ONE of the
    // two values (never a torn interleave), with no temp residue.
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("config.toml");
    let dir = tmp.path().to_path_buf();

    let t1_target = target.clone();
    let t1 = std::thread::spawn(move || {
        for _ in 0..50 {
            write_atomically(&t1_target, b"aaa-threads").unwrap();
        }
    });
    let t2_target = target.clone();
    let t2 = std::thread::spawn(move || {
        for _ in 0..50 {
            write_atomically(&t2_target, b"bbb-threads").unwrap();
        }
    });
    t1.join().unwrap();
    t2.join().unwrap();

    let final_bytes = std::fs::read(&target).unwrap();
    assert!(
        final_bytes == b"aaa-threads" || final_bytes == b"bbb-threads",
        "the published bytes must be exactly one writer's value: {final_bytes:?}"
    );
    assert!(
        temp_residue(&dir).is_empty(),
        "100 concurrent writes must leave no temp residue"
    );
}
