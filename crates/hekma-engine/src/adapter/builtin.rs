//! The engine's builtin native-adapter table (spine AD-3).
//!
//! Maps a native `kind` string to a compiled-in [`AgentAdapter`]. This table is
//! how `kt agent register --kind <kind>` resolves a native adapter in the
//! **shipping** engine.
//!
//! ## Why the mock lives here, not in `hekma-conformance`
//!
//! The `mock` kind resolves to [`BuiltinMock`], defined in the engine. It is NOT
//! the `hekma-conformance` `MockAdapter`, even though they share the same
//! declared shape: a normal `engine → conformance` dependency edge would be
//! transitive into `kt` (`kt → engine → conformance`) and appear in
//! `cargo tree -p ktesio -e normal,build`, tripping the AD-2 boundary gate. The
//! conformance mock stays a dev/test fixture (imported as a dev-dependency by
//! tests that need the richer reusable version); this builtin is what ships so a
//! real operator's `--kind mock` works.
//!
//! Lifecycle verbs stay on the trait's inert default bodies for the mock; the
//! `hermes` builtin (story 6-2) carries a code-declared launch so the engine's
//! start seam can spawn it.

use hekma_adapter_api::{
    AgentAdapter, Capability, CapabilityDeclaration, ConfigMapping, ConfigTarget, MeteringSource,
    OsId, SupportLevel,
};

use crate::adapter::StartLaunch;

/// The builtin `mock`'s code-declared config mapping (story 2-2, AC3/AC8): the
/// documented unified key `model` → the ENV var `MODEL`. `env` is the clean,
/// directly-assertable native target the inert-mock proof observes on the mapped
/// launch (Decision 4/8). Kept in shape-parity with the conformance `MockAdapter`
/// (the cross-boundary parity test guards it).
pub const MOCK_MODEL_ENV_VAR: &str = "MODEL";

/// The builtin `mock`'s code-declared env target for the RESERVED
/// [`MEMORY_DIR_KEY`] leaf (story 5-1, Task 5.4 lockstep): the engine injects the
/// managed Memory Backing directory path at `memory.dir` at start, and the mock
/// maps it to this ENV var so the descriptor has a declared native mechanism.
/// MUST stay in lockstep with the conformance `MockAdapter` — the parity test
/// (`conformance_mock_fixture_matches_builtin_shape`) fails if only one moves.
pub const MOCK_MEMORY_ENV_VAR: &str = "KTESIO_MEMORY_DIR";

/// The `hermes` builtin's code-declared launch (story 6-2, CP-b), re-exported
/// from the adapter crate so the engine owns the resolution while the adapter
/// owns the declaration. Foreground gateway under Ktesio's ProcessBackend;
/// `--external-supervisor` makes in-chat restarts exit 75 — to the engine that
/// is just a non-zero exit while Running (the ordinary crash → on-failure
/// relaunch reuses the SAME persisted snapshot; no special case).
pub const HERMES_EXEC: &str = hekma_adapters_hermes::HERMES_EXEC;
pub const HERMES_ARGS: [&str; 3] = hekma_adapters_hermes::HERMES_ARGS;

/// Resolve a native `kind` to a boxed builtin adapter, or `None` if unknown.
///
/// The table carries three kinds: the inert `mock` (the conformance
/// stand-in), the launchable `hermes` builtin (story 6-2, the first
/// launchable native adapter), and the launchable `acp` builtin (story 14-1,
/// spine AD-19 — the transport core; its START launch comes from the
/// instance's `acp.command`/`acp.args` config keys at start time, so unlike
/// hermes it declares no code-declared launch here).
pub fn native(kind: &str) -> Option<Box<dyn AgentAdapter>> {
    match kind {
        "mock" => Some(Box::new(BuiltinMock::new())),
        hekma_adapters_hermes::HERMES_KIND => {
            Some(Box::new(hekma_adapters_hermes::HermesAdapter::new()))
        }
        crate::acp::ACP_KIND => Some(Box::new(BuiltinAcp::new())),
        _ => None,
    }
}

/// The code-declared `start` [`StartLaunch`] for a launchable native `kind`, or
/// `None` when the kind has no process to spawn (`mock`) or is unknown.
///
/// Story 6-2 lifts the "native builtins cannot start" limitation: `resolve`
/// captures this into the registration snapshot and `resolve_start_launch`
/// consults it BEFORE erroring, so a native instance starts from the SAME
/// persisted-launch path as a manifest adapter. The `Option` stays honest —
/// most native kinds remain inert.
pub fn native_launch(kind: &str) -> Option<StartLaunch> {
    match kind {
        hekma_adapters_hermes::HERMES_KIND => Some(StartLaunch {
            exec: HERMES_EXEC.to_string(),
            args: HERMES_ARGS.iter().map(|s| s.to_string()).collect(),
            env: std::collections::BTreeMap::new(),
        }),
        _ => None,
    }
}

/// The code-declared config [`ConfigMapping`] for a native `kind`, or `None` if
/// the kind is unknown (story 2-2). This is how the engine's start seam obtains a
/// NATIVE adapter's mapping (a manifest adapter's mapping comes from its parsed
/// `[config]` section instead). A known native adapter that maps no unified keys
/// returns an EMPTY mapping (its trait default). Reuses the same [`native`] table
/// so the mapping can never drift from the adapter that declares it.
pub fn native_config_mapping(kind: &str) -> Option<ConfigMapping> {
    native(kind).map(|adapter| adapter.config_mapping())
}

/// The engine's builtin `mock` adapter (shipping counterpart of the conformance
/// fixture; identical declared shape).
///
/// Declares `pause` guaranteed on Linux/macOS and best-effort on Windows (the
/// AD-4 exemplar) and `interaction` guaranteed everywhere, with a
/// self-reported Metering Source so it registers successfully.
#[derive(Clone, Debug)]
struct BuiltinMock {
    capabilities: CapabilityDeclaration,
}

impl BuiltinMock {
    fn new() -> Self {
        let capabilities = CapabilityDeclaration::new()
            .with(Capability::Pause, OsId::Linux, SupportLevel::Guaranteed)
            .with(Capability::Pause, OsId::Macos, SupportLevel::Guaranteed)
            .with(Capability::Pause, OsId::Windows, SupportLevel::BestEffort)
            .with(
                Capability::Interaction,
                OsId::Linux,
                SupportLevel::Guaranteed,
            )
            .with(
                Capability::Interaction,
                OsId::Macos,
                SupportLevel::Guaranteed,
            )
            .with(
                Capability::Interaction,
                OsId::Windows,
                SupportLevel::Guaranteed,
            );
        Self { capabilities }
    }
}

impl AgentAdapter for BuiltinMock {
    fn kind(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> &CapabilityDeclaration {
        &self.capabilities
    }

    fn metering_source(&self) -> MeteringSource {
        MeteringSource::SelfReported
    }

    /// The code-declared unified→native config mapping (story 2-2): `model` → the
    /// ENV var [`MOCK_MODEL_ENV_VAR`], plus — since story 5-1 (Task 5.4 lockstep) —
    /// the reserved `memory.dir` key → [`MOCK_MEMORY_ENV_VAR`] so a filesystem
    /// Memory Backing has a declared native mechanism. Mirrors the conformance
    /// `MockAdapter` so the fixture stays a faithful stand-in (the parity test
    /// guards it).
    fn config_mapping(&self) -> ConfigMapping {
        ConfigMapping::new()
            .with("model", ConfigTarget::env(MOCK_MODEL_ENV_VAR))
            .with(
                crate::domain::MEMORY_DIR_KEY,
                ConfigTarget::env(MOCK_MEMORY_ENV_VAR),
            )
    }

    // Lifecycle ops use the trait's inert default bodies (execution is 1-4).
}

/// The engine's builtin `acp` adapter (story 14-1, spine AD-19): the
/// registration half of the ACP transport core.
///
/// * **Metering Source = `SelfReported`** (the contract requires a viable
///   source; the sentinel channel yields honest nothing for agents that do
///   not emit — the designed last-resort honesty, AI-18/AD-8). The tiered
///   acquisition (observed base-URL / sentinel mode / the honest `—` gap
///   notice) is stories 14-3/14-5; `usage_update` surfacing is 14-3. This
///   story implements the transport core only.
/// * **Capabilities:** `interaction` GUARANTEED on every OS (the ACP
///   transport IS the stdin/stdout pipe pair — without a piped stdin there
///   is no transport) and `pause` guaranteed on Linux/macOS, best-effort on
///   Windows — pause/resume keep PROCESS semantics for an `acp` instance
///   (SIGSTOP/SIGCONT parity; the transport is untouched by a frozen
///   process).
/// * **No code-declared launch:** the launch comes from the instance's
///   `acp.command`/`acp.args` unified config keys at start ([`crate::acp`]'s
///   resolver; the start refuses honestly naming both keys when unset), so
///   `native_launch("acp")` stays `None` and the start's acp branch resolves
///   the launch from the effective config instead of the builtin table.
/// * **No `contract_version` negotiation:** a builtin does not negotiate
///   (epic-6 B3 precedent) — the acp kind never engages the adapter contract
///   v1 (D5).
/// * **Config mapping:** empty (no unified key maps into a native target;
///   `agent.*` pass-through still applies through the generic start seam).
#[derive(Clone, Debug)]
struct BuiltinAcp {
    capabilities: CapabilityDeclaration,
}

impl BuiltinAcp {
    fn new() -> Self {
        let capabilities = CapabilityDeclaration::new()
            .with(Capability::Pause, OsId::Linux, SupportLevel::Guaranteed)
            .with(Capability::Pause, OsId::Macos, SupportLevel::Guaranteed)
            .with(Capability::Pause, OsId::Windows, SupportLevel::BestEffort)
            .with(
                Capability::Interaction,
                OsId::Linux,
                SupportLevel::Guaranteed,
            )
            .with(
                Capability::Interaction,
                OsId::Macos,
                SupportLevel::Guaranteed,
            )
            .with(
                Capability::Interaction,
                OsId::Windows,
                SupportLevel::Guaranteed,
            );
        Self { capabilities }
    }
}

impl AgentAdapter for BuiltinAcp {
    fn kind(&self) -> &str {
        crate::acp::ACP_KIND
    }

    fn capabilities(&self) -> &CapabilityDeclaration {
        &self.capabilities
    }

    fn metering_source(&self) -> MeteringSource {
        MeteringSource::SelfReported
    }

    // Config mapping: the trait's EMPTY default (the acp kind maps no
    // unified keys; `agent.*` pass-through delivery is unchanged).
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_kind_resolves() {
        let adapter = native("mock").expect("mock must resolve");
        assert_eq!(adapter.kind(), "mock");
        assert_eq!(adapter.metering_source(), MeteringSource::SelfReported);
        assert!(!adapter.capabilities().is_empty());
    }

    #[test]
    fn unknown_kind_returns_none() {
        assert!(native("nope").is_none());
        assert!(native("").is_none());
    }

    #[test]
    fn acp_kind_resolves_with_the_transport_shape() {
        // Story 14-1: the launchable acp builtin resolves through the same
        // table as mock/hermes. Metering source = self-reported (the honest
        // last resort; the tiered billing acquisition is 14-3/14-5).
        let adapter = native(crate::acp::ACP_KIND).expect("acp must resolve");
        assert_eq!(adapter.kind(), crate::acp::ACP_KIND);
        assert_eq!(adapter.metering_source(), MeteringSource::SelfReported);
        let decl = adapter.capabilities();
        for os in [OsId::Linux, OsId::Macos, OsId::Windows] {
            // The transport IS the stdio pipe pair: interaction guaranteed
            // everywhere; pause keeps process semantics (guaranteed on
            // Linux/macOS, best-effort on Windows — the AD-4 exemplar shape).
            assert_eq!(
                decl.support(Capability::Interaction, os),
                SupportLevel::Guaranteed,
                "os={os}"
            );
            assert_eq!(
                decl.support(Capability::Pause, os),
                if os == OsId::Windows {
                    SupportLevel::BestEffort
                } else {
                    SupportLevel::Guaranteed
                },
                "os={os}"
            );
        }
        // No code-declared launch: the launch resolves from the instance's
        // acp.command/acp.args config keys at start (the start's acp branch),
        // so the builtin table's launch stays None — a registration snapshot
        // with no launch, exactly like mock.
        assert!(native_launch(crate::acp::ACP_KIND).is_none());
        // No unified-key mapping (empty mapping; pass-through unchanged).
        assert_eq!(
            native_config_mapping(crate::acp::ACP_KIND).unwrap().len(),
            0
        );
    }

    #[test]
    fn hermes_kind_resolves_with_declared_shape() {
        // Story 6-2: the launchable native builtin resolves through the same
        // table as `mock`, carrying its CP-a/d declared shape.
        let adapter = native(hekma_adapters_hermes::HERMES_KIND).expect("hermes must resolve");
        assert_eq!(adapter.kind(), hekma_adapters_hermes::HERMES_KIND);
        assert_eq!(adapter.metering_source(), MeteringSource::SelfReported);
        let decl = adapter.capabilities();
        for os in [OsId::Linux, OsId::Macos, OsId::Windows] {
            assert_eq!(
                decl.support(Capability::Pause, os),
                SupportLevel::BestEffort
            );
            assert_eq!(
                decl.support(Capability::Interaction, os),
                SupportLevel::Guaranteed
            );
        }
        // Only the reserved memory.dir leaf maps — to HERMES_HOME (CP-e+f);
        // `model` is a documented no-op (Decision 6). The KEY is named via the
        // engine's own constant (same discipline as the mock test below), so a
        // rename of the reserved key fails here too, not just in production.
        let mapping = adapter.config_mapping();
        assert_eq!(mapping.len(), 1);
        assert_eq!(
            mapping
                .target(crate::domain::MEMORY_DIR_KEY)
                .unwrap()
                .env_var(),
            Some(hekma_adapters_hermes::HERMES_HOME)
        );
        assert!(mapping.target("model").is_none());
    }

    #[test]
    fn native_launch_carries_the_hermes_gateway_launch_and_nothing_for_mock() {
        // Story 6-2 (DC-1): hermes declares its foreground gateway launch in
        // code; mock stays inert.
        let launch =
            native_launch(hekma_adapters_hermes::HERMES_KIND).expect("hermes must carry a launch");
        assert_eq!(launch.exec, HERMES_EXEC);
        assert_eq!(launch.exec, hekma_adapters_hermes::HERMES_KIND);
        assert_eq!(launch.args, vec!["gateway", "run", "--external-supervisor"]);
        assert!(launch.env.is_empty());
        assert!(native_launch("mock").is_none());
        assert!(native_launch("nope").is_none());
    }

    #[test]
    fn builtin_mock_declares_the_model_and_memory_env_mappings() {
        // Story 2-2 (AC3/AC8): the builtin mock code-declares `model` → env
        // `MODEL`. Story 5-1 adds the reserved `memory.dir` → env
        // `KTESIO_MEMORY_DIR` mapping so a filesystem Memory Backing has a
        // declared native mechanism.
        let adapter = native("mock").unwrap();
        let mapping = adapter.config_mapping();
        assert_eq!(mapping.len(), 2);
        assert_eq!(
            mapping.target("model").unwrap().env_var(),
            Some(MOCK_MODEL_ENV_VAR)
        );
        assert_eq!(
            mapping
                .target(crate::domain::MEMORY_DIR_KEY)
                .unwrap()
                .env_var(),
            Some(MOCK_MEMORY_ENV_VAR)
        );
        // An unmapped documented key has no rule (delivered nowhere — a no-op).
        assert!(mapping.target("temperature").is_none());
    }

    #[test]
    fn builtin_mock_declares_the_ad4_exemplar_shape() {
        // Intra-crate sanity check of the builtin's literal per-OS shape. The
        // REAL cross-boundary guard that the builtin and the conformance
        // MockAdapter agree lives in `crates/hekma-engine/tests/registration.rs`
        // (`conformance_mock_fixture_matches_builtin_shape`), which can see both
        // (conformance is a dev-dependency of the test target); this test alone
        // cannot reference conformance without tripping the AD-2 boundary gate.
        let adapter = native("mock").unwrap();
        let decl = adapter.capabilities();
        assert_eq!(
            decl.support(Capability::Pause, OsId::Linux),
            SupportLevel::Guaranteed
        );
        assert_eq!(
            decl.support(Capability::Pause, OsId::Windows),
            SupportLevel::BestEffort
        );
        assert_eq!(
            decl.support(Capability::Interaction, OsId::Macos),
            SupportLevel::Guaranteed
        );
    }
}
