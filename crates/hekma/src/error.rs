use miette::Diagnostic;
use thiserror::Error;

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::self_update::failed))]
pub struct SelfUpdateFailed {
    pub message: String,
}

/// Story 17-2: another host already serves the state directory (the
/// `host.lock` refusal). The message names the live holder PID + the lock
/// path + the remediation (from `hekma_host::ServeError::StateDirLocked`'s
/// Display — carried verbatim, never re-worded here). Classified as exit `2`
/// (Usage — the invocation cannot proceed on this state root; no new
/// exit-code number was minted, DC-4).
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::serve::state_dir_busy))]
pub struct ServeStateDirBusy {
    pub message: String,
}

/// Story 17-2: a serve OPTION value is invalid — today a `--stop-window`
/// outside the 1..=86400 whole-second bounds (review finding 9; the message
/// is carried verbatim from `hekma_host::ServeError::InvalidOptions`). The
/// CLI's own clap range refuses these at parse time; this diagnostic covers
/// the programmatic/host path. Classified as exit `2` (Usage — the flag
/// value, not the environment, is at fault).
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::serve::options_invalid))]
pub struct ServeOptionsInvalid {
    pub message: String,
}

/// Story 17-2: the host could not start (state-dir resolution/creation,
/// lock I/O, engine open, runtime build, signal registration). The message
/// is the surfaced reason, carried verbatim from
/// `hekma_host::ServeError::Startup`'s Display. Classified as exit `1`
/// (review finding 8: an ENVIRONMENT/system failure — filesystem, OS
/// resource — per the house exit table's code-1 class, NOT a usage error;
/// the second-owner refusal and invalid options stay exit `2`).
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::serve::startup_failed))]
pub struct ServeStartupFailed {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::duplicate_name))]
pub struct AgentDuplicateName {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::invalid_name))]
pub struct AgentInvalidName {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::not_found))]
pub struct AgentNotFound {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::running_requires_force))]
pub struct AgentRunningRequiresForce {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::io))]
pub struct AgentIo {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::store))]
pub struct AgentStore {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::unknown_kind))]
pub struct AgentUnknownKind {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::manifest_not_found))]
pub struct AgentManifestNotFound {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::manifest_invalid))]
pub struct AgentManifestInvalid {
    pub message: String,
}

/// A manifest targets a different Adapter Contract MAJOR than the engine speaks
/// (story 6-6, FR-30). The message names BOTH versions and quotes the
/// compatibility rule; classified as exit `1` (the general/internal catch-all —
/// the frozen 4-3 exit-code table gained no new number).
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::contract_incompatible))]
pub struct AgentContractIncompatible {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::manifest_unreadable))]
pub struct AgentManifestUnreadable {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::no_metering_source))]
pub struct AgentNoMeteringSource {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::no_capabilities))]
pub struct AgentNoCapabilities {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::invalid_transition))]
pub struct AgentInvalidTransition {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::launch_failed))]
pub struct AgentLaunchFailed {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::capability_unsupported))]
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
#[diagnostic(code(hekma::agent::resume_unsupported))]
pub struct AgentResumeUnsupported {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::unknown_config_key))]
pub struct AgentUnknownConfigKey {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::config))]
pub struct AgentConfig {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::not_running))]
pub struct AgentNotRunning {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::interaction_unavailable))]
pub struct AgentInteractionUnavailable {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::interaction_timed_out))]
pub struct AgentInteractionTimedOut {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::stop_unconfirmed))]
pub struct AgentStopUnconfirmed {
    pub message: String,
}

/// Story 14-1 (spine AD-19): a second `send` targeted an `acp` instance while
/// a turn was already in flight. The acp kind serializes turns per session —
/// a concurrent prompt is REFUSED, never queued. Classified as exit `4`
/// (Invalid state) — the instance is fine, the timing is not; no new
/// exit-code number was minted (DC-4).
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::acp_turn_in_flight))]
pub struct AgentAcpTurnInFlight {
    pub message: String,
}

/// Story 12-1: a detached start (`hekma agent start --detach`) was requested for
/// an instance whose configuration cannot be detached — v1's single refusal is
/// the `engine-observed` metering channel, whose loopback forward listener
/// lives inside the starting command and would strand the agent's model traffic
/// on a dead port the moment the command exits. The engine refuses before any
/// side effect (no transition, no listener, no spawn), and the message carries
/// the why + the remediation. Classified as exit `5` (Unsupported) — the
/// operation cannot be done for this instance, the same class as a
/// capability-unsupported command; no new exit-code number was minted (DC-4).
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::detach_refused))]
pub struct AgentDetachRefused {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::memory_hot_swap))]
pub struct AgentMemoryHotSwap {
    pub message: String,
}

#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::memory_kind_conflict))]
pub struct AgentMemoryKindConflict {
    pub message: String,
}

/// Story 18-4: the adapter's declared lifecycle COMMAND (`[lifecycle.pause]` /
/// `[lifecycle.resume]`) failed — it exited nonzero, could not spawn, or
/// outlived its bound — so the pause/resume was NOT applied and the instance's
/// durable state is untouched (never a fake pause). The command line is
/// rendered (no environment — no-leak); the detail names the failure mode.
/// Classified as exit `1` (General) through the documented catch-all: the
/// adapter's own command failed, which is neither a usage error nor a
/// capability refusal.
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::pause_command_failed))]
pub struct AgentPauseCommandFailed {
    pub message: String,
}

/// Story 18-4: a drain bound outside `1..=600000` ms was requested
/// programmatically (the CLI's own clap range refuses it at parse time —
/// this diagnostic covers the embedding path, the same shape as
/// [`ServeOptionsInvalid`]). Classified as exit `2` (Usage — the bound
/// value, not the environment, is at fault); no new exit-code number was
/// minted (DC-4).
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::agent::drain_bound_invalid))]
pub struct AgentDrainBoundInvalid {
    pub message: String,
}

/// Story 21-2: no Run has the id (exit `3`, the not-found class).
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::run::not_found))]
pub struct RunNotFound {
    pub message: String,
}

/// Story 21-2: the Run request cannot be honored as given — nesting past the
/// cap, a Breach Action the child's depth cannot execute, a malformed label
/// or limit (exit `2`, the usage class).
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::run::invalid))]
pub struct RunInvalid {
    pub message: String,
}

/// Story 21-2: the Run's (or its parent's) state forbids the request — no
/// open Run on the instance, an ended parent, a process-owning Run closed
/// (exit `4`, the invalid-state class).
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::run::invalid_state))]
pub struct RunInvalidState {
    pub message: String,
}

/// Story 21-4: the local Service Host (AD-20's one data-path owner) could not
/// be found or started for the state root, or stopped answering after it was
/// found. The message names the state root and `serve.log` where the host's
/// own output lands. Classified as exit `1` (General) through the documented
/// catch-all — an environment/system failure, like `ServeStartupFailed`; no
/// new exit-code number was minted (DC-4).
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::host::unavailable))]
pub struct ServiceHostUnavailable {
    pub message: String,
}

/// Story 21-4: the local Service Host refused a CLI write with an error that
/// is not an engine error this build can decode (a request rejection, or an
/// engine error from a newer host). The message is the host's own. Exit `1`
/// (General) through the catch-all, like `ServiceHostUnavailable`.
#[derive(Error, Diagnostic, Debug)]
#[error("{}", message)]
#[diagnostic(code(hekma::host::refused))]
pub struct ServiceHostRefused {
    pub message: String,
}
