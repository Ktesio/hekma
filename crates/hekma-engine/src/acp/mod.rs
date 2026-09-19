//! The ACP transport core (spine AD-19, epic-14 story 14-1): the engine
//! speaks the Agent Client Protocol as a CLIENT over a `--kind acp`
//! instance's stdio — newline-delimited JSON-RPC 2.0 (UTF-8, one message per
//! `\n`, stdout purity), hand-rolled over `serde_json` ONLY (NO new crates;
//! the official ACP SDK was rejected for v1 under NFR-8 — churn vs. the
//! single-integer protocol version).
//!
//! Story 14-1 delivers the TRANSPORT CORE only — the module family:
//!
//! * [`codec`] — ndJSON + JSON-RPC 2.0 envelopes, the tolerated protocol
//!   version set `{1}`, the DEFAULT client capabilities (advertise-nothing,
//!   D3), and the request-params builders.
//! * [`updates`] — the tolerant `session/update` parser: the v1
//!   discriminator set plus a counted `Unhandled` catch-all;
//!   `usage_update` is CONTEXT-grain and is SURFACED ONLY — never minted
//!   into the billing ledger (the two usage grains must never mix; the
//!   billing tiers are stories 14-3/14-6).
//! * [`client`] — the bounded `initialize` → `session/new` handshake, the
//!   version-tolerance refusal, and the `session/request_permission`
//!   denial policy.
//! * [`connection`] — the reader thread, the bounded sole-writer stdin
//!   path, request-response correlation, the one-in-flight-turn flag, the
//!   permission auto-denial, the surfaced-notice queue, and the teardown
//!   (close stdin → reader EOF) on stop/drop/terminal-settle.
//!
//! Stories 14-6/14-3/14-2/14-5/14-4 are OTHER stories: cached tokens, the
//! tiered billing acquisition, session-id persistence + `session/load`
//! adoption, the sentinel mode, and the verification/docs sweep — none of
//! them live here.
//!
//! This module is crate-INTERNAL (the transport is engine machinery; the
//! public surface change for 14-1 is the `acp` builtin kind's registration
//! availability only). OS-uniform (AD-4): everything here is cfg-free —
//! ndJSON framing is identical on every platform.

mod client;
mod codec;
mod connection;
mod updates;

use crate::domain::{EffectiveConfig, EngineError, InstanceName};
use crate::domain::{ACP_ARGS_KEY, ACP_COMMAND_KEY};

pub(crate) use client::handshake;
pub(crate) use connection::{AcpConnection, PromptError};

/// The builtin `acp` kind (spine AD-19) — the native-adapter table key the
/// `--kind acp` registration resolves through. The same `^[a-z0-9][a-z0-9_-]*$`
/// token rule as every builtin kind (the spine's adapter-kind convention).
pub(crate) const ACP_KIND: &str = "acp";

/// Whether `kind` is the builtin `acp` kind (the supervisor's one dispatch
/// point for the transport-specific start/send/stop behavior).
pub(crate) fn is_acp_kind(kind: &str) -> bool {
    kind == ACP_KIND
}

/// Resolve an `acp` instance's START LAUNCH from its unified config keys
/// (spine AD-19 / the epic-14 I/O matrix): `acp.command` (REQUIRED — the
/// executable to run) plus `acp.args` (OPTIONAL). The `acp` builtin declares
/// NO code-declared launch (unlike `hermes`) because the launch is
/// per-instance operator configuration — so `start` refuses HONESTLY,
/// naming both keys, when `acp.command` is unset.
///
/// `acp.args` is TOLERANTLY typed: a TOML array of strings is used
/// element-wise; a plain string is split on whitespace (a value carrying
/// spaces cannot be expressed that way — hand-edit the instance
/// `config.toml` with an array form for that); any other shape is a typed
/// refusal. The resolution is PURE (a config read + validation) and runs
/// BEFORE any persisted state change, so a refusal rejects the start with
/// no spurious transition.
pub(crate) fn resolve_acp_launch(
    name: &InstanceName,
    effective: &EffectiveConfig,
) -> Result<crate::adapter::StartLaunch, EngineError> {
    let refuse = |detail: String| EngineError::AdapterUnresolved {
        name: name.as_str().to_string(),
        detail,
    };
    let command = match effective.value(ACP_COMMAND_KEY) {
        Some(toml::Value::String(command)) if !command.trim().is_empty() => {
            command.trim().to_string()
        }
        Some(other) => {
            return Err(refuse(format!(
                "the config key '{ACP_COMMAND_KEY}' must be a non-empty string (the executable \
                 to run); got {other}"
            )))
        }
        None => {
            return Err(refuse(format!(
                "no launch command is configured for the acp kind; set the config keys \
                 '{ACP_COMMAND_KEY}' (required, the ACP agent executable) and '{ACP_ARGS_KEY}' \
                 (optional, its arguments) — e.g. `kt agent config set {name} {ACP_COMMAND_KEY} \
                 /path/to/agent`",
                name = name.as_str(),
            )))
        }
    };
    let args = match effective.value(ACP_ARGS_KEY) {
        None => Vec::new(),
        Some(toml::Value::Array(items)) => {
            let mut args = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    toml::Value::String(arg) => args.push(arg.clone()),
                    other => {
                        return Err(refuse(format!(
                            "the config key '{ACP_ARGS_KEY}' must be an array of strings; an \
                             element is {other}"
                        )))
                    }
                }
            }
            args
        }
        Some(toml::Value::String(value)) => value
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>(),
        Some(other) => {
            return Err(refuse(format!(
                "the config key '{ACP_ARGS_KEY}' must be an array of strings (or a \
                 whitespace-separated string); got {other}"
            )))
        }
    };
    Ok(crate::adapter::StartLaunch {
        exec: command,
        args,
        env: std::collections::BTreeMap::new(),
    })
}

/// Whether THIS spawn should pipe the child's stdout to the engine (the ACP
/// transport needs the protocol stream) — true exactly for an `acp` kind
/// that is NOT detached (a detached spawn gets `Stdio::null()` stdin and no
/// transport, mirroring the detached interaction refusal; see
/// [`SpawnSpec::pipe_stdin`] for the detach rationale).
pub(crate) fn spawn_pipe_stdout(is_acp: bool, detach: bool) -> bool {
    is_acp && !detach
}

/// The honest adoption note (story 14-1): an ADOPTED `acp` instance is
/// re-held as a bare process — its ACP pipe halves died with the engine
/// that spawned it, so this engine holds NO connection for it (the
/// session/load resume is story 14-2; until then `send` fails with the
/// ordinary adopted-instance interaction error). The diagnostic names the
/// condition + the remediation, surfaced-not-silent (AI-18).
pub(crate) fn adopted_acp_note(name: &str) -> String {
    format!(
        "{name}: adopted an acp instance; the ACP connection is a pair of pipes that died \
         with the previous engine process, so THIS engine holds no ACP session for it — \
         send will refuse until the instance is stopped and started again (session resume \
         across engine lifetimes lands in story 14-2)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ConfigLayer, SourceLayer};
    use toml::Value;

    /// An `EffectiveConfig` from a single instance-layer TOML body (the
    /// other layers empty) — the resolution input.
    fn effective_from_instance(body: &str) -> EffectiveConfig {
        let layers = [
            ConfigLayer::empty(),
            ConfigLayer::empty(),
            ConfigLayer::parse(SourceLayer::Instance, "<test>", body).unwrap(),
            ConfigLayer::empty(),
        ];
        crate::domain::resolve(layers)
    }

    fn launch_of(body: &str) -> Result<crate::adapter::StartLaunch, EngineError> {
        let name = InstanceName::new("acp-1").unwrap();
        resolve_acp_launch(&name, &effective_from_instance(body))
    }

    #[test]
    fn launch_resolves_from_the_command_and_args_keys() {
        // The string-form args (whitespace-split) is the CLI `config set`
        // path; an absolute command resolves verbatim.
        let launch =
            launch_of("acp.command = '/usr/bin/some-acp-agent'\nacp.args = '--mode chunky'\n")
                .unwrap();
        assert_eq!(launch.exec, "/usr/bin/some-acp-agent");
        assert_eq!(launch.args, vec!["--mode", "chunky"]);
        assert!(launch.env.is_empty());
    }

    #[test]
    fn launch_resolves_the_toml_array_args_form() {
        // The hand-edited config.toml array form carries args with spaces.
        let launch =
            launch_of("acp.command = 'agent'\nacp.args = [\"--model\", \"gpt 4\"]\n").unwrap();
        assert_eq!(launch.args, vec!["--model", "gpt 4"]);
    }

    #[test]
    fn missing_or_empty_command_refuses_naming_both_keys() {
        // Absent: the refusal NAMES both keys (the I/O matrix's honest
        // refusal) and suggests the config set remediation.
        let err = launch_of("").unwrap_err();
        let text = err.to_string();
        assert!(text.contains(ACP_COMMAND_KEY), "{text}");
        assert!(text.contains(ACP_ARGS_KEY), "{text}");
        // An empty command is the same refusal shape (a present-but-empty
        // value is not a launch).
        let err = launch_of("acp.command = '   '\n").unwrap_err();
        assert!(err.to_string().contains(ACP_COMMAND_KEY));
        // A non-string command is a typed refusal.
        let err = launch_of("acp.command = 7\n").unwrap_err();
        assert!(err.to_string().contains("non-empty string"), "{err}");
    }

    #[test]
    fn malformed_args_refuse_typed() {
        // A non-string array element.
        let err = launch_of("acp.command = 'agent'\nacp.args = [1]\n").unwrap_err();
        assert!(err.to_string().contains(ACP_ARGS_KEY), "{err}");
        // A non-array, non-string shape.
        let err = launch_of("acp.command = 'agent'\nacp.args = 3\n").unwrap_err();
        assert!(err.to_string().contains(ACP_ARGS_KEY), "{err}");
        // Absent args are fine (empty argv).
        let launch = launch_of("acp.command = 'agent'\n").unwrap();
        assert!(launch.args.is_empty());
    }

    #[test]
    fn kind_dispatch_and_the_adoption_note_are_stable() {
        assert!(is_acp_kind("acp"));
        assert!(!is_acp_kind("mock"));
        assert!(!is_acp_kind("hermes"));
        // The adoption note names the instance, the dead-pipe fact, and the
        // 14-2 remediation horizon (surfaced-not-silent).
        let note = adopted_acp_note("acp-1");
        assert!(note.contains("acp-1"), "{note}");
        assert!(note.contains("14-2"), "{note}");
        // The transport pipes stdout exactly for a non-detached acp spawn.
        assert!(spawn_pipe_stdout(true, false));
        assert!(!spawn_pipe_stdout(true, true));
        assert!(!spawn_pipe_stdout(false, false));
        // Silence an unused-import lint in the test module (Value is used
        // only through the toml literals above on some compilers).
        let _ = Value::from(1);
    }
}
