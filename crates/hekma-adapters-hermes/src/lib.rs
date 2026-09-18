//! # hekma-adapters-hermes
//!
//! Native (in-workspace Rust) adapter for the NousResearch [Hermes Agent],
//! implementing the Adapter Contract from `hekma-adapter-api` (architecture
//! spine AD-2, AD-3). This is the FIRST launchable native adapter: the engine's
//! builtin table carries its launch (`hermes gateway run --external-supervisor`,
//! foreground under Ktesio's ProcessBackend — verification note CP-b), while a
//! native builtin previously had no process to spawn (the `mock` stays inert).
//!
//! ## Declared shape (story 6-2, ratified from CP-6.1-a…f)
//!
//! - **Pause = BestEffort on every OS** (CP-a): the gateway has no signal-freeze
//!   mechanism; its ESTOP sentinel only stops NEW work. The engine surfaces this
//!   honestly with the `pause-best-effort` / `resume-best-effort` qualifier
//!   causes — no engine change needed.
//! - **Interaction = Guaranteed on every OS** (FR-22): the gateway pipes stdin.
//! - **MeteringSource::SelfReported** (CP-d): usage comes from the agent's own
//!   `/usage` + insights surface; BudgetEvaluator stays additive (no $ cap).
//! - **Config mapping**: ONLY the reserved unified key `memory.dir` → env
//!   [`HERMES_HOME`] (CP-e+f) — the same filesystem-
//!   backing invocation override the builtin mock maps to `KTESIO_MEMORY_DIR`.
//!   The `model` key is deliberately UNMAPPED (Decision 6): Hermes switches
//!   models via its own `hermes model` CLI, so an operator-set `model` value is
//!   delivered nowhere (a silent no-op).
//!
//! [Hermes Agent]: https://github.com/NousResearch/hermes-agent

/// The reserved unified config key `memory.dir`, restated as a literal.
///
/// The engine-side constant lives in the engine's domain and is NOT exported
/// across the adapter boundary (AD-2 keeps `hekma-adapters-hermes` depending
/// only on `hekma-adapter-api`), so the adapter declares its own literal copy.
/// A drift would fail the engine-side composition test (the mapping lookup by
/// the engine's key would find nothing).
pub const MEMORY_DIR_KEY_LITERAL: &str = "memory.dir";

/// The ENV var the reserved `memory.dir` leaf maps to (CP-e+f): the engine
/// injects the managed Memory Backing directory path at start into
/// [`HERMES_HOME`], which sits at the TOP of the agent's home-resolution chain —
/// pointing each instance at its own home inside its Agent Home satisfies the
/// one-agent-per-home constraint and keeps state inside the managed dir.
///
/// An instance WITHOUT filesystem Memory Backing receives NO `HERMES_HOME`
/// override at all; the agent then falls back to its own default chain (its
/// unmanaged default home). That fallback is documented behavior, not an error.
pub const HERMES_HOME: &str = "HERMES_HOME";

/// The code-declared launch for the `hermes` kind (story 6-2, CP-b): run the
/// gateway FOREGROUND under Ktesio's ProcessBackend with
/// `--external-supervisor`. In-chat restarts/updates then exit with code 75 so
/// the external supervisor (Ktesio) relaunches; to the engine that hand-off is
/// just a non-zero exit while Running — the ordinary crash → on-failure relaunch
/// path reuses the SAME persisted launch snapshot. No special case anywhere.
pub const HERMES_EXEC: &str = "hermes";

/// Positional args of the foreground gateway launch (see [`HERMES_EXEC`]).
pub const HERMES_ARGS: [&str; 3] = ["gateway", "run", "--external-supervisor"];

/// The adapter's kind string (review blind-7): the single source of truth for
/// the name every resolution path matches against — the engine's builtin table
/// matches this constant in its `match` arms (a `&str` const IS a valid match
/// pattern), and [`AgentAdapter::kind`] returns it, so the kind name can never
/// drift between the table and the declaration.
pub const HERMES_KIND: &str = HERMES_EXEC;

use hekma_adapter_api::{
    AgentAdapter, Capability, CapabilityDeclaration, ConfigMapping, ConfigTarget, MeteringSource,
    OsId, SupportLevel,
};

/// The native adapter for the NousResearch Hermes Agent.
///
/// Declared shape per the module docs; lifecycle verbs stay on the trait's
/// default bodies (the engine drives processes through its own backends —
/// adapters declare, the engine executes).
#[derive(Clone, Debug)]
pub struct HermesAdapter {
    capabilities: CapabilityDeclaration,
}

impl HermesAdapter {
    pub fn new() -> Self {
        let capabilities = CapabilityDeclaration::new()
            // CP-a: pause is best-effort EVERYWHERE — new-work-only (the gateway's
            // ESTOP sentinel), never a signal freeze of in-flight turns.
            .with(Capability::Pause, OsId::Linux, SupportLevel::BestEffort)
            .with(Capability::Pause, OsId::Macos, SupportLevel::BestEffort)
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

impl Default for HermesAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAdapter for HermesAdapter {
    fn kind(&self) -> &str {
        HERMES_KIND
    }

    fn capabilities(&self) -> &CapabilityDeclaration {
        &self.capabilities
    }

    /// Self-reported usage (CP-d): the agent reports its own token counts via
    /// its `/usage` + insights surfaces; the engine ingests them additively.
    fn metering_source(&self) -> MeteringSource {
        MeteringSource::SelfReported
    }

    /// The code-declared unified→native config mapping: ONLY the reserved
    /// `memory.dir` leaf → env [`HERMES_HOME`] (CP-e+f, composed exactly like
    /// the mock's `KTESIO_MEMORY_DIR`). The documented `model` key is
    /// deliberately unmapped (Decision 6 — see module docs).
    fn config_mapping(&self) -> ConfigMapping {
        ConfigMapping::new().with(MEMORY_DIR_KEY_LITERAL, ConfigTarget::env(HERMES_HOME))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_kind_resolves() {
        let adapter = HermesAdapter::new();
        assert_eq!(adapter.kind(), HERMES_KIND);
        assert_eq!(adapter.metering_source(), MeteringSource::SelfReported);
        assert!(!adapter.capabilities().is_empty());
    }

    #[test]
    fn hermes_declares_pause_best_effort_on_every_os() {
        // CP-a: best-effort pause everywhere — never signal-pause.
        let decl = HermesAdapter::new().capabilities;
        for os in [OsId::Linux, OsId::Macos, OsId::Windows] {
            assert_eq!(
                decl.support(Capability::Pause, os),
                SupportLevel::BestEffort,
                "pause must be best-effort on {os:?}"
            );
        }
    }

    #[test]
    fn hermes_declares_interaction_guaranteed_on_every_os() {
        let decl = HermesAdapter::new().capabilities;
        for os in [OsId::Linux, OsId::Macos, OsId::Windows] {
            assert_eq!(
                decl.support(Capability::Interaction, os),
                SupportLevel::Guaranteed,
                "interaction must be guaranteed on {os:?}"
            );
        }
    }

    #[test]
    fn hermes_maps_memory_dir_to_hermes_home_and_nothing_else() {
        let mapping = HermesAdapter::new().config_mapping();
        assert_eq!(mapping.len(), 1, "only memory.dir is mapped");
        assert_eq!(
            mapping.target("memory.dir").unwrap().env_var(),
            Some("HERMES_HOME")
        );
        // Decision 6: `model` is delivered NOWHERE (silent no-op).
        assert!(mapping.target("model").is_none());
    }
}
