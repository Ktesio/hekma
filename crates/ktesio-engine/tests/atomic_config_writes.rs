//! Story 11-2 (AI-24/AI-28) + review-1 patches: the ATOMIC config-write
//! durability contract, exercised at the real seams through the PUBLIC API.
//!
//! The failure injections here need a READ-ONLY directory (chmod), which is a
//! Unix-only OS call — so the tests are `#[cfg(unix)]`-gated, under the OS-cfg
//! gate's integration-test allowlist (`crates/ktesio-engine/tests/`, the AI-35
//! disclosure convention). Cross-platform halves of the same contract (temp
//! collision safety, rename failure, residue cleanup) are pinned OS-agnostically
//! in `src/paths.rs` and `src/adapter/mod.rs` unit tests; what is honestly
//! Windows-missing here is only the chmod-style injection, not the contract.
//!
//! Three scenarios:
//! 1. a `config set` whose atomic write fails (read-only home directory)
//!    leaves the OLD bytes intact and no temp residue (AI-24);
//! 2. a hand-tightened `0600` mode survives a re-set — permission
//!    preservation across the atomic flip (review-1 patch 3), both through
//!    the bare helper and the full `set_config` seam;
//! 3. a failed config-FILE render leaves the PREVIOUS native file
//!    byte-identical (AI-28).

use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use ktesio_adapter_api::{ConfigMapping, ConfigTarget};
use ktesio_engine::paths::write_atomically;
use ktesio_engine::{ConfigLayer, Engine, SourceLayer};
use tempfile::TempDir;

/// Every directory entry under `dir` whose name carries the temp marker.
fn temp_residue(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp-"))
        .collect()
}

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// An [`ktesio_engine::EffectiveConfig`] whose single INSTANCE-layer leaf is
/// `model = "<value>"` — the minimal resolved config the mapping seam needs
/// (the same shape the in-module `effective_from_instance` helper builds).
fn effective_with_model(value: &str) -> ktesio_engine::EffectiveConfig {
    let layers = [
        ConfigLayer::empty(),
        ConfigLayer::empty(),
        ConfigLayer::parse(
            SourceLayer::Instance,
            "<test>",
            &format!("model = \"{value}\"\n"),
        )
        .unwrap(),
        ConfigLayer::empty(),
    ];
    ktesio_engine::domain::resolve(layers)
}

#[cfg(unix)]
#[test]
fn config_set_write_failure_leaves_old_bytes_and_no_temp_residue() {
    // AI-24 at the full `set_config` seam: with the home directory READ-ONLY
    // the layer read still succeeds (r+x) but the atomic temp write fails —
    // the typed MalformedLayer surfaces, the OLD bytes are intact, and no
    // temp residue appears (the helper could not create anything).
    let state = TempDir::new().unwrap();
    let engine = Engine::open(Some(state.path().to_path_buf())).unwrap();
    let facade = engine.blocking();
    facade.register("demo", "mock").unwrap();

    facade.set_config("demo", "model", "old-value").unwrap();
    let home = state.path().join("agents").join("demo");
    let config = home.join("config.toml");
    let old_bytes = std::fs::read(&config).unwrap();

    chmod(&home, 0o555);
    let err = facade.set_config("demo", "model", "new-value").unwrap_err();
    chmod(&home, 0o755); // restore before drop regardless of the asserts below

    let detail = err.to_string();
    assert!(
        detail.contains("could not write"),
        "the write failure must surface as the write error; {detail}"
    );
    assert_eq!(
        std::fs::read(&config).unwrap(),
        old_bytes,
        "a failed atomic write must leave the OLD bytes"
    );
    assert!(
        temp_residue(&home).is_empty(),
        "a failed atomic write must leave no temp residue"
    );
}

#[cfg(unix)]
#[test]
fn set_config_preserves_a_tightened_0600_mode() {
    // Review-1 patch 3, at the full seam: a hand-tightened 0600 config.toml
    // survives a re-set — the atomic helper copies the target's mode onto the
    // temp before the rename instead of publishing the process-default mode.
    let state = TempDir::new().unwrap();
    let engine = Engine::open(Some(state.path().to_path_buf())).unwrap();
    let facade = engine.blocking();
    facade.register("demo", "mock").unwrap();

    facade.set_config("demo", "model", "first").unwrap();
    let config = state.path().join("agents").join("demo").join("config.toml");
    chmod(&config, 0o600);

    facade.set_config("demo", "model", "second").unwrap();

    let mode = std::fs::metadata(&config).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o777,
        0o600,
        "the tightened 0600 mode must survive the atomic re-set"
    );
    // The content really did update (the mode was preserved, not the file).
    assert!(
        std::fs::read_to_string(&config).unwrap().contains("second"),
        "the re-set must still persist the new value"
    );
}

#[cfg(unix)]
#[test]
fn write_atomically_preserves_the_target_mode() {
    // Review-1 patch 3, at the bare helper: overwriting an existing target
    // keeps its mode; the content updates; no residue.
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("config.toml");

    write_atomically(&target, b"first").unwrap();
    chmod(&target, 0o600);
    write_atomically(&target, b"second").unwrap();

    let mode = std::fs::metadata(&target).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "the 0600 mode must survive the flip");
    assert_eq!(std::fs::read(&target).unwrap(), b"second");
    assert!(temp_residue(tmp.path()).is_empty());
}

#[cfg(unix)]
#[test]
fn apply_file_target_failed_write_leaves_the_previous_native_file_unchanged() {
    // AI-28, the scenario half: a native config file that ALREADY holds a
    // previous render is never truncated by a FAILED re-render. With the
    // render directory READ-ONLY the second render's temp write fails — the
    // previous bytes survive byte-identically and no temp residue appears.
    let tmp = TempDir::new().unwrap();
    let mapping = ConfigMapping::new().with("model", ConfigTarget::file("agent.toml", "k"));

    // First render succeeds (the normal path) — this is the "previous file".
    let first = effective_with_model("first-value");
    let mut launch = ktesio_engine::adapter::StartLaunch {
        exec: "unused".into(),
        args: Vec::new(),
        env: BTreeMap::new(),
    };
    ktesio_engine::adapter::apply_config_mapping(
        &mut launch,
        &mapping,
        &first,
        &BTreeMap::new(),
        tmp.path(),
    )
    .unwrap();
    let target = tmp.path().join("agent.toml");
    let previous_bytes = std::fs::read(&target).unwrap();
    assert!(!previous_bytes.is_empty(), "the first render landed");

    // Second render fails: the render directory is read-only.
    chmod(tmp.path(), 0o555);
    let second = effective_with_model("second-value");
    let mut launch = ktesio_engine::adapter::StartLaunch {
        exec: "unused".into(),
        args: Vec::new(),
        env: BTreeMap::new(),
    };
    let err = ktesio_engine::adapter::apply_config_mapping(
        &mut launch,
        &mapping,
        &second,
        &BTreeMap::new(),
        tmp.path(),
    )
    .unwrap_err();
    chmod(tmp.path(), 0o755); // restore before drop

    assert!(
        matches!(
            err,
            ktesio_engine::adapter::ConfigApplyError::FileRender { .. }
        ),
        "the failed re-render must be a typed FileRender; got {err:?}"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        previous_bytes,
        "a failed atomic render must leave the PREVIOUS native file byte-identical"
    );
    assert!(temp_residue(tmp.path()).is_empty());
}
