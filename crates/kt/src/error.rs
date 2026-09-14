use miette::Diagnostic;
use thiserror::Error;

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::self_update::failed))]
pub struct SelfUpdateFailed {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::duplicate_name))]
pub struct AgentDuplicateName {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::invalid_name))]
pub struct AgentInvalidName {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::not_found))]
pub struct AgentNotFound {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::running_requires_force))]
pub struct AgentRunningRequiresForce {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::io))]
pub struct AgentIo {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::store))]
pub struct AgentStore {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::unknown_kind))]
pub struct AgentUnknownKind {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::manifest_not_found))]
pub struct AgentManifestNotFound {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::manifest_invalid))]
pub struct AgentManifestInvalid {
    pub message: String,
}

/// A manifest targets a different Adapter Contract MAJOR than the engine speaks
/// (story 6-6, FR-30). The message names BOTH versions and quotes the
/// compatibility rule; classified as exit `1` (the general/internal catch-all —
/// the frozen 4-3 exit-code table gained no new number).
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::contract_incompatible))]
pub struct AgentContractIncompatible {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::manifest_unreadable))]
pub struct AgentManifestUnreadable {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::no_metering_source))]
pub struct AgentNoMeteringSource {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::no_capabilities))]
pub struct AgentNoCapabilities {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::invalid_transition))]
pub struct AgentInvalidTransition {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::launch_failed))]
pub struct AgentLaunchFailed {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::capability_unsupported))]
pub struct AgentCapabilityUnsupported {
    pub message: String,
}

/// AI-7: `resume` targeted a PAUSED instance whose adapter declares PAUSE
/// unsupported on this OS — the engine cannot confidently signal the suspension
/// awake. A dedicated diagnostic (not the bare pause-unsupported one) because
/// the instance is already `paused`: the bare diagnostic would strand the
/// operator with no way forward. The message names the state + the declaration
/// and gives the escape hatch (`stop` works without pause support). Classified
/// as exit `5` (Unsupported capability) — the same class as
/// [`AgentCapabilityUnsupported`], no new exit-code number (DC-4).
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::resume_unsupported))]
pub struct AgentResumeUnsupported {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::unknown_config_key))]
pub struct AgentUnknownConfigKey {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::config))]
pub struct AgentConfig {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::not_running))]
pub struct AgentNotRunning {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::interaction_unavailable))]
pub struct AgentInteractionUnavailable {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::interaction_timed_out))]
pub struct AgentInteractionTimedOut {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::stop_unconfirmed))]
pub struct AgentStopUnconfirmed {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::memory_hot_swap))]
pub struct AgentMemoryHotSwap {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(ktesio::agent::memory_kind_conflict))]
pub struct AgentMemoryKindConflict {
    pub message: String,
}
