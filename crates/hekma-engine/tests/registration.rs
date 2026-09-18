//! Integration test: register → list → remove through the engine's PUBLIC API
//! only (spine AD-2 "public API is sufficient" proof for the registration
//! capability).
//!
//! Everything here uses only items re-exported from the crate root. If this
//! file needs a `pub(crate)` item, that is a signal the public surface is
//! insufficient — it must compile against the public API alone.

use std::path::PathBuf;

use hekma_engine::{
    AdapterRef, AgentInstance, InstanceName, LifecycleState, Registry, RegistryError,
    RemoveDisposition,
};
use tempfile::TempDir;

/// Open a registry against a fresh temp state dir (no env, explicit base).
fn open(base: &TempDir) -> Registry {
    Registry::open(Some(base.path().to_path_buf())).expect("open registry")
}

#[test]
fn register_list_remove_full_cycle_via_public_api() {
    let tmp = TempDir::new().unwrap();
    let reg = open(&tmp);

    // Fresh state: empty Fleet.
    assert!(reg.list().unwrap().is_empty());

    // Register two instances of the same kind.
    let alpha: AgentInstance = reg.register("alpha", "mock").unwrap();
    let beta: AgentInstance = reg.register("beta", "mock").unwrap();
    assert_eq!(alpha.state, LifecycleState::Registered);
    assert_eq!(beta.state, LifecycleState::Registered);

    // The reported Agent Home paths are engine-computed, distinct, and exist.
    assert_ne!(alpha.agent_home, beta.agent_home);
    assert!(PathBuf::from(&alpha.agent_home).is_dir());
    assert!(PathBuf::from(&beta.agent_home).is_dir());

    // List returns both, ordered by name.
    let names: Vec<String> = reg
        .list()
        .unwrap()
        .into_iter()
        .map(|i| i.name.into())
        .collect();
    assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);

    // Empty Usage Ledger for a fresh instance.
    let alpha_name = InstanceName::new("alpha").unwrap();
    assert_eq!(reg.usage_event_count(&alpha_name).unwrap(), 0);

    // Remove beta with delete; alpha's home stays byte-identical (isolation).
    let alpha_home = PathBuf::from(&alpha.agent_home);
    let alpha_config_before = std::fs::read(alpha_home.join("config.toml")).unwrap();
    reg.remove("beta", RemoveDisposition::Delete, false)
        .unwrap();
    let alpha_config_after = std::fs::read(alpha_home.join("config.toml")).unwrap();
    assert_eq!(alpha_config_before, alpha_config_after);
    assert!(!PathBuf::from(&beta.agent_home).exists());

    // Only alpha remains.
    let remaining: Vec<String> = reg
        .list()
        .unwrap()
        .into_iter()
        .map(|i| i.name.into())
        .collect();
    assert_eq!(remaining, vec!["alpha".to_string()]);
}

#[test]
fn duplicate_registration_surfaces_typed_error_via_public_api() {
    let tmp = TempDir::new().unwrap();
    let reg = open(&tmp);
    reg.register("demo", "mock").unwrap();
    // Re-register the same NAME (kind must resolve, so reuse `mock`); the
    // duplicate is detected after adapter resolution passes (story 1.3).
    let err = reg.register("demo", "mock").unwrap_err();
    assert!(matches!(err, RegistryError::DuplicateName { name } if name == "demo"));
}

#[test]
fn state_persists_across_reopen() {
    // A second Registry over the same base sees the prior registration — proves
    // migration-on-reopen is idempotent through the public API.
    let tmp = TempDir::new().unwrap();
    {
        let reg = open(&tmp);
        reg.register("persisted", "mock").unwrap();
    }
    let reopened = open(&tmp);
    let names: Vec<String> = reopened
        .list()
        .unwrap()
        .into_iter()
        .map(|i| i.name.into())
        .collect();
    assert_eq!(names, vec!["persisted".to_string()]);
}

// ---- Story 1.3: adapter resolution via the PUBLIC API only ----

/// A complete valid `adapter.toml` written into a manifest directory fixture.
const FIXTURE_MANIFEST: &str = r#"
contract_version = "1.0.0"

[adapter]
kind = "fixture"

[lifecycle.start]
exec = "fixture-agent"

[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "best-effort"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"
"#;

#[test]
fn register_native_and_manifest_and_read_effective_declaration_via_public_api() {
    // AD-2 proof: register a native `mock` AND a manifest adapter through the
    // public API only, then read back each effective declaration.
    let tmp = TempDir::new().unwrap();
    let reg = open(&tmp);

    // Native mock.
    let mock = reg.register("demo", "mock").unwrap();
    assert_eq!(mock.kind, "mock");
    let mock_caps = reg.effective_capabilities("demo").unwrap();
    assert!(!mock_caps.is_empty());

    // Manifest adapter from a temp directory fixture.
    let manifest_dir = TempDir::new().unwrap();
    std::fs::write(manifest_dir.path().join("adapter.toml"), FIXTURE_MANIFEST).unwrap();
    let m = reg
        .register_with_adapter(
            "m",
            &AdapterRef::Manifest(manifest_dir.path().to_path_buf()),
        )
        .unwrap();
    assert_eq!(m.kind, "fixture");
    assert_eq!(m.state, LifecycleState::Registered);
    let m_caps = reg.effective_capabilities("m").unwrap();
    assert!(!m_caps.is_empty());
    // The projection is for the running OS (data-driven, works on every host).
    assert_eq!(m_caps.os, hekma_engine::OsId::current());
}

#[test]
fn manifest_no_metering_rejected_via_public_api_leaves_no_partial_state() {
    // AC4 hard line proven through the public API.
    let tmp = TempDir::new().unwrap();
    let reg = open(&tmp);

    let manifest_dir = TempDir::new().unwrap();
    let body = FIXTURE_MANIFEST.replace("[metering]\nsource = \"self-reported\"\n", "");
    std::fs::write(manifest_dir.path().join("adapter.toml"), body).unwrap();

    let err = reg
        .register_with_adapter(
            "m",
            &AdapterRef::Manifest(manifest_dir.path().to_path_buf()),
        )
        .unwrap_err();
    assert!(err.to_string().contains("[metering]"), "got {err}");
    assert!(
        reg.list().unwrap().is_empty(),
        "no partial state on AC4 reject"
    );
}

#[test]
fn conformance_mock_fixture_matches_builtin_shape() {
    // F2 — the REAL mock-drift guard. The shipping builtin `mock` (resolved via
    // the engine's public API) and the reusable conformance `MockAdapter` fixture
    // (a DEV-dependency here) MUST declare the identical per-OS Capability
    // Declaration. This is the single cross-boundary equality that protects the
    // whole BuiltinMock/MockAdapter duplication: if either drifts, this fails.
    //
    // Both are compared directly (both derive PartialEq) AND cell-by-cell across
    // Capability::ALL × OsId::MODELED, so a divergence on any OS/capability is
    // caught even on a single-OS CI runner (the matrix is data).
    use hekma_adapter_api::{AgentAdapter, Capability, OsId};

    // The shipping builtin's declaration, obtained through the public resolve
    // path (kt never depends on conformance; this test is the dev-side guard).
    let builtin = hekma_engine::adapter::resolve(&AdapterRef::Native("mock".to_string()))
        .expect("builtin mock resolves");
    let builtin_declaration = builtin.declaration();

    // The conformance fixture's declaration.
    let fixture = hekma_conformance::MockAdapter::new();
    let fixture_declaration = fixture.capabilities();

    // Whole-declaration equality (PartialEq) — the primary guard.
    assert_eq!(
        builtin_declaration, fixture_declaration,
        "shipping builtin `mock` and the conformance MockAdapter fixture declarations diverged"
    );

    // Cell-by-cell across every capability × modeled OS, as data (belt and
    // suspenders — pinpoints WHICH cell drifted if the equality above ever fails).
    for capability in Capability::ALL {
        for os in OsId::MODELED {
            assert_eq!(
                builtin_declaration.support(capability, os),
                fixture_declaration.support(capability, os),
                "mock drift at capability={capability} os={os}"
            );
        }
    }

    // Story 2-2: the two mocks must ALSO declare the identical config MAPPING
    // (the shipping builtin's mapping via the public native table; the fixture's
    // via its `config_mapping()` accessor). Both declare `model → env MODEL`; this
    // guards the mapping half of the BuiltinMock/MockAdapter duplication against
    // drift, exactly like the capability equality above.
    let builtin_mapping = hekma_engine::adapter::native_config_mapping("mock")
        .expect("builtin mock has a config mapping");
    let fixture_mapping = fixture.config_mapping();
    assert_eq!(
        builtin_mapping, fixture_mapping,
        "shipping builtin `mock` and the conformance MockAdapter config mappings diverged"
    );

    // The fixture is inert this story (execution is 1-4).
    assert!(fixture.scripted_fake_agent().is_inert());
}
