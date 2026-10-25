mod cli;
mod error;
mod exit_code;
mod install_channel;
mod ui;
mod update_check;

use clap::{CommandFactory, Parser, Subcommand};

// The license is a custom one (LICENSE, `license-file` in the manifests), so
// Cargo exposes no `CARGO_PKG_LICENSE` value for it (that env is empty under
// `license-file`); the title is stated literally instead.
const HELP_FOOTER: &str = concat!(
    "License: Apache License Version 2.0 (Apache-2.0)",
    "\nRepository: ",
    env!("CARGO_PKG_REPOSITORY")
);

const SELF_UPDATE_AFTER_HELP: &str = "\
Details:
  Updates the hekma binary using the current install channel. Homebrew installs run
  brew upgrade, Cargo installs run cargo install --force, and manual binary
  installs download and verify the latest GitHub Release archive.

Example:
  hekma self-update";

const SERVE_AFTER_HELP: &str = "\
Details:
  Runs the long-running Service Host over one state directory: adopts every
  live agent at open, then waits. On SIGTERM (or Ctrl+C) every RUNNING agent
  is stopped through the bounded graceful ladder (--stop-window <secs>,
  default 30, bounded 1..=86400), each outcome surfaced, and only then does
  the host exit. Hosted children inherit ONLY their manifest-declared
  environment plus a minimal OS-baseline floor (process plumbing — Unix
  PATH/HOME; Windows SystemRoot/SystemDrive/ComSpec/TEMP/TMP/PATH), never
  the host shell's ambient variables. A second `hekma serve` on the same
  state directory is refused naming the holder PID (exit 2); a crashed
  host's stale lock file re-acquires.

Examples:
  hekma serve
  hekma serve --stop-window 10
  hekma serve --metering-listen 8400 --state-dir ~/agents-state
  hekma serve --listen 127.0.0.1:8477 --bearer-secret CONTROL_BEARER

The Control API (--listen + --bearer-secret, both or neither) exposes the
unstable v0 HTTP/JSON surface — fleet, per-instance lifecycle/config/status,
and events (SSE + cursor pull) — guarded by the resolved bearer token; see
https://hekma.ktesio.dev/docs/control-api/.";

const AGENT_AFTER_HELP: &str = "\
Details:
  An agent is a third-party program you run — a personal agent such as Hermes
  Agent or OpenClaw, or a coding agent such as OpenCode or GitHub Copilot CLI —
  that hekma launches, supervises, meters, and budgets as a process.
  Manages Agent Instances in the Fleet. register creates an isolated Agent Home
  under a unique name from a native adapter (--kind) or a manifest adapter
  (--manifest <dir-or-file>), validating its Capability Declaration and Metering
  Source before any state is written; it prints the Agent Home path and the
  effective per-OS Capability Declaration. start launches a registered instance
  (its state becomes running); stop requests a graceful shutdown and escalates to
  a forced kill after the window (--timeout <secs>, default 30), leaving no
  surviving process. pause/resume suspend and resume a running instance with
  honest per-OS semantics: a guaranteed pause really suspends the process (SIGSTOP
  on Unix), a best-effort pause proceeds cooperatively and prints a visible
  qualifier note, and an unsupported pause fails fast quoting the Capability
  Declaration. remove deletes the registry entry and, with --delete, the Agent
  Home too (--retain, the default, keeps it); list shows the Fleet; show renders
  one instance's effective capabilities and runtime status. Both list and show
  accept --json for a machine-readable document (usage is real token totals from
  the Usage Ledger; an instance with no budget set shows '—' in the table and null
  in JSON). config set writes a key to the Agent Instance layer (validated at write
  time — an unknown key outside the agent.* pass-through namespace is rejected
  with the nearest valid key suggested, and nothing is persisted); config get
  prints the effective (resolved) config, where a key set at the instance layer
  overrides the same key at the kind/engine-default layer, every time;
  each value names its source layer (a Source column, or a source field with
  --json), and starting an instance persists an effective-config snapshot in the
  Agent Home. A secret:NAME value is resolved from the environment or the engine
  secrets file at start and delivered to the agent, but is MASKED in config get,
  the snapshot, logs, and events; config get --reveal is the sole way to
  print it unmasked. Removing a running instance requires --force.

Examples:
  hekma agent register demo --kind mock
  hekma agent register my-agent --manifest ./my-agent
  hekma agent start my-agent
  hekma agent pause my-agent
  hekma agent resume my-agent
  hekma agent stop my-agent --timeout 10
  hekma agent show demo
  hekma agent list
  hekma agent list --json
  hekma agent config set demo model gpt-4
  hekma agent config set demo agent.api_key secret:OPENAI_KEY
  hekma agent config get demo
  hekma agent config get demo model
  hekma agent config get demo --json
  hekma agent config get demo --reveal
  hekma agent remove demo --delete";

#[derive(Parser)]
#[command(
    name = "hekma",
    version,
    about = "Run AI agents like services — supervise, meter, and budget them",
    after_help = HELP_FOOTER
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Update the hekma binary
    #[command(
        name = "self-update",
        about = "Update the hekma binary",
        after_help = SELF_UPDATE_AFTER_HELP
    )]
    SelfUpdate,
    /// Manage Agent Instances in the Fleet
    #[command(
        about = "Manage Agent Instances in the Fleet",
        after_help = AGENT_AFTER_HELP
    )]
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },
    /// Serve a state directory as a long-running host
    #[command(
        about = "Serve a state directory as a long-running host",
        after_help = SERVE_AFTER_HELP
    )]
    Serve {
        /// Graceful stop window in seconds for the shutdown drain (default 30)
        #[arg(
            long,
            value_name = "SECS",
            value_parser = clap::value_parser!(u64).range(1..=86_400)
        )]
        stop_window: Option<u64>,
        /// Fixed loopback port for observed metering listeners (default 0 =
        /// OS-ephemeral). A host restart re-binds the SAME port, so adopted
        /// observed instances stay live; a port already in use refuses the
        /// start (never a silent hop).
        #[arg(long, value_name = "PORT", default_value_t = 0)]
        metering_listen: u16,
        /// Serve this state directory instead of the resolved default
        /// (HEKMA_STATE_DIR, the legacy KTESIO_STATE_DIR, or the platform data dir)
        #[arg(long, value_name = "DIR")]
        state_dir: Option<String>,
        /// Expose the v0 Control API on this address (e.g. 127.0.0.1:8477).
        /// Default: off. Requires --bearer-secret (the API never serves
        /// unauthenticated).
        #[arg(
            long,
            value_name = "ADDR",
            value_parser = clap::value_parser!(std::net::SocketAddr),
            requires = "bearer_secret"
        )]
        listen: Option<std::net::SocketAddr>,
        /// The NAME of the bearer secret guarding the Control API — resolved
        /// once at startup from the environment (the NAME as the env var),
        /// then the state dir's secrets file. Requires --listen.
        #[arg(long, value_name = "NAME", requires = "listen")]
        bearer_secret: Option<String>,
        // The default lives in `serve::run` (`unwrap_or_default`): there is
        // no clap `default_value`, so exactly one site defaults and `None`
        // (no flag) stays distinguishable from an explicit `--log-capture full`.
        /// Log capture for every agent this host supervises: full (default)
        /// keeps all output; metadata keeps only sizes and hashes at rest; off
        /// captures nothing and disables usage reported in logs.
        #[arg(long, value_name = "MODE")]
        log_capture: Option<hekma_engine::LogCaptureMode>,
    },
}

#[derive(Subcommand)]
enum AgentCommands {
    /// Register a new Agent Instance under a unique name
    Register {
        /// Fleet-unique instance name (`^[a-z0-9][a-z0-9_-]*$`)
        name: String,
        /// Native adapter kind (e.g. mock). Mutually exclusive with --manifest.
        #[arg(
            long,
            conflicts_with = "manifest",
            required_unless_present = "manifest"
        )]
        kind: Option<String>,
        /// Path to a manifest adapter directory (or adapter.toml file).
        /// Mutually exclusive with --kind.
        #[arg(long)]
        manifest: Option<String>,
    },
    /// Remove an Agent Instance from the Fleet
    Remove {
        /// Name of the Agent Instance to remove
        name: String,
        /// Delete the Agent Home directory as well
        #[arg(long, conflicts_with = "retain")]
        delete: bool,
        /// Keep the Agent Home directory on disk (default)
        #[arg(long)]
        retain: bool,
        /// Remove even if the instance is running
        #[arg(long)]
        force: bool,
    },
    /// Start a registered Agent Instance
    Start {
        /// Name of the Agent Instance to start
        name: String,
        /// Detach: keep the agent running after this command exits; the next
        /// `hekma` command re-adopts it. Between commands there is no
        /// crash detection, no budget enforcement, and no event delivery:
        /// supervision lasts only while a command runs. Refused for
        /// engine-observed instances (their loopback listener stops with this
        /// command).
        #[arg(long)]
        detach: bool,
    },
    /// Stop a running Agent Instance (graceful, then forced after the window)
    Stop {
        /// Name of the Agent Instance to stop
        name: String,
        /// Graceful-shutdown window in seconds before a forced kill (default 30)
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Pause a running Agent Instance (honest per-OS: guaranteed/best-effort/unsupported)
    Pause {
        /// Name of the Agent Instance to pause
        name: String,
    },
    /// Resume a paused Agent Instance
    Resume {
        /// Name of the Agent Instance to resume
        name: String,
    },
    /// Drain a running Agent Instance: pause it via its adapter's declared
    /// `[lifecycle.pause]` COMMAND (cooperative — no OS signal), wait —
    /// bounded — for the declared idle signal, then stop it. A drain that
    /// outlives its bound STILL stops (the outcome is reported honestly).
    Drain {
        /// Name of the Agent Instance to drain
        name: String,
        /// Drain bound in milliseconds (1..=600000). Overrides the manifest's
        /// `[drain].timeout_ms`; when both are absent the engine default
        /// (30s) applies.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=600_000))]
        timeout_ms: Option<u64>,
    },
    /// Send text input to a running Agent Instance's native input channel
    ///
    /// Text that looks like a flag, such as "-5 degrees" or "--help", is sent
    /// as input, so --help does not work here; run `hekma agent help send`
    /// to see this help.
    // `--help`/`-h` are disabled on this subcommand and `text` accepts
    // leading-hyphen values, so a payload that looks like a flag is delivered
    // literally. Intercepting it as help would print help and exit 0 without
    // sending anything, and a caller checking only the exit code would wrongly
    // believe the send succeeded.
    #[command(disable_help_flag = true)]
    Send {
        /// Name of the Agent Instance to send input to
        name: String,
        /// The text to send (a trailing newline is added if missing). A value
        /// that starts with - or --, such as "-5 degrees", is sent as is.
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// Read an Agent Instance's retained output, optionally following live output
    Logs {
        /// Name of the Agent Instance to read logs for
        name: String,
        /// After the one-shot dump, keep polling and print new lines as they
        /// arrive; exits cleanly (with a note) once the instance stops or pauses
        #[arg(long, short = 'f')]
        follow: bool,
        /// Print each captured line as one versioned JSON object per line
        /// (NDJSON), with the same shape with or without --follow. Notes and
        /// diagnostics go to stderr.
        #[arg(long)]
        json: bool,
    },
    /// Report Usage Ledger token totals (and derived dollar cost) for one
    /// instance, or Fleet-wide totals when no name is given
    Usage {
        /// Optional Agent Instance name; omitted reports the Fleet-wide totals
        name: Option<String>,
        /// Emit the usage snapshot as a machine-readable JSON document
        #[arg(long)]
        json: bool,
    },
    /// List every Agent Instance in the Fleet
    List {
        /// Emit the Fleet as a machine-readable JSON document
        #[arg(long)]
        json: bool,
    },
    /// Show an Agent Instance's effective per-OS Capability Declaration
    Show {
        /// Name of the Agent Instance to inspect
        name: String,
        /// Emit the instance's runtime status as a machine-readable JSON document
        #[arg(long)]
        json: bool,
    },
    /// Get or set an Agent Instance's unified config (layered)
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Attach or detach an Agent Instance's Memory Backing
    Memory {
        #[command(subcommand)]
        command: MemoryCommands,
    },
    /// Print the governance conformance vectors — the versioned,
    /// SHA-256-checksummed decision artifact a Host pins to prove its
    /// external guard matches the engine's budget/cost rules
    GovernanceVectors {
        /// Emit the raw artifact bytes verbatim (byte-identical to the
        /// released `vectors-v1.json` — the pin target for a checksum match)
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum MemoryCommands {
    /// Attach a Memory Backing to an Agent Instance (requires a terminal state —
    /// no hot-swap). `filesystem` creates the managed directory inside the Agent
    /// Home; `native` records the delegation (Hekma guarantees only Agent Home
    /// persistence) and creates nothing.
    Attach {
        /// Name of the Agent Instance
        name: String,
        /// The Memory Backing kind to attach (accepted: filesystem, native)
        #[arg(long)]
        kind: String,
        /// Emit the attachment as a machine-readable JSON document
        #[arg(long)]
        json: bool,
    },
    /// Detach an Agent Instance's Memory Backing (metadata only: the managed
    /// directory and its contents remain on disk; requires a terminal state)
    Detach {
        /// Name of the Agent Instance
        name: String,
        /// Emit the detachment as a machine-readable JSON document
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCommands {
    /// Set a config key on the Agent Instance layer (validated at write time)
    Set {
        /// Name of the Agent Instance
        name: String,
        /// Config key (a known unified key, or an `agent.*` pass-through key)
        key: String,
        /// Value to set, stored as given. A `secret:NAME` value stores the
        /// reference; the secret is resolved at start and masked when read. A
        /// value that starts with - or --, such as -x, is taken as the value,
        /// so a -- separator is optional.
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Get an Agent Instance's effective (resolved) config value(s) with per-value source
    Get {
        /// Name of the Agent Instance
        name: String,
        /// Optional config key; omitted prints the whole effective config
        key: Option<String>,
        /// Emit the effective config (value + source layer per leaf) as JSON
        #[arg(long)]
        json: bool,
        /// Reveal secret values unmasked (the only way to print them). Without
        /// it, `secret:` values are masked in both the table and --json.
        /// Never un-masks the persisted snapshot, logs, or events. Secrets are
        /// resolved again at read time (env, then the secrets file), so a
        /// revealed value may differ from what a running instance resolved at
        /// its start.
        #[arg(long)]
        reveal: bool,
    },
}

/// Run one CLI invocation: parse args, maybe emit the passive update
/// notice, dispatch the subcommand. Shared by BOTH shipped binaries
/// (`hekma`, the primary name, and `hkm`, the short alias) — they are
/// the same program, so there is exactly one CLI implementation.
#[cfg(not(tarpaulin_include))]
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    if should_check_for_updates(&cli.command) {
        if let Some(notice) = update_check::maybe_notice() {
            ui::update_notice(&notice.current_version, &notice.latest_version);
        }
    }

    match cli.command {
        Some(Commands::SelfUpdate) => cli::self_update::run(),
        Some(Commands::Serve {
            stop_window,
            metering_listen,
            state_dir,
            listen,
            bearer_secret,
            log_capture,
        }) => cli::serve::run(
            stop_window,
            metering_listen,
            state_dir,
            listen,
            bearer_secret,
            // Review finding 19: the flag passes through as an Option —
            // `serve::run`'s single `unwrap_or_default` is THE defaulting
            // site (no clap `default_value` duplicates it).
            log_capture,
        ),
        Some(Commands::Agent { command }) => match command {
            AgentCommands::Register {
                name,
                kind,
                manifest,
            } => {
                let adapter = cli::agent::AdapterArg::from_flags(kind, manifest)?;
                cli::agent::register(&name, &adapter)
            }
            AgentCommands::Remove {
                name,
                delete,
                retain,
                force,
            } => cli::agent::remove(
                &name,
                cli::agent::DispositionArg::from_flags(delete, retain),
                force,
            ),
            AgentCommands::Start { name, detach } => cli::agent::start(&name, detach),
            AgentCommands::Stop { name, timeout } => cli::agent::stop(&name, timeout),
            AgentCommands::Pause { name } => cli::agent::pause(&name),
            AgentCommands::Resume { name } => cli::agent::resume(&name),
            AgentCommands::Drain { name, timeout_ms } => cli::agent::drain(&name, timeout_ms),
            AgentCommands::Send { name, text } => cli::agent::send(&name, &text),
            AgentCommands::Logs { name, follow, json } => cli::agent::logs(&name, follow, json),
            AgentCommands::Usage { name, json } => cli::agent::usage(name.as_deref(), json),
            AgentCommands::List { json } => cli::agent::list(json),
            AgentCommands::Show { name, json } => cli::agent::show(&name, json),
            AgentCommands::Config { command } => match command {
                ConfigCommands::Set { name, key, value } => {
                    cli::agent::config_set(&name, &key, &value)
                }
                ConfigCommands::Get {
                    name,
                    key,
                    json,
                    reveal,
                } => cli::agent::config_get(&name, key.as_deref(), json, reveal),
            },
            AgentCommands::Memory { command } => match command {
                MemoryCommands::Attach { name, kind, json } => {
                    cli::agent::memory_attach(&name, &kind, json)
                }
                MemoryCommands::Detach { name, json } => cli::agent::memory_detach(&name, json),
            },
            AgentCommands::GovernanceVectors { json } => cli::agent::governance_vectors(json),
        },
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

fn should_check_for_updates(command: &Option<Commands>) -> bool {
    matches!(command, Some(command) if !matches!(command, Commands::SelfUpdate | Commands::Serve { .. }))
}

/// Classify a returned diagnostic into the documented, stable numeric exit
/// code (story 4-3, FR-26 / PRD §7), print it, and terminate the process.
/// clap's own usage/parse errors already exit `2` and `--help`/`--version`
/// exit `0` from inside `Cli::parse()`, so they never reach here; this maps
/// only the runtime diagnostics, with any unmapped error falling to `1`
/// (preserving the pre-4-3 behavior). See `exit_code::classify`.
#[cfg(not(tarpaulin_include))]
pub fn handle_error_and_exit(err: Box<dyn std::error::Error>) -> ! {
    let code = exit_code::classify(err.as_ref());
    ui::error(err);
    std::process::exit(code.code());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_struct_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn test_cli_subcommands_exist() {
        let cmd = Cli::command();
        // The two agent-runner-relevant top-level commands are PRESENT.
        assert!(cmd.find_subcommand("self-update").is_some());
        assert!(cmd.find_subcommand("agent").is_some());
        // Single canonical Fleet surface (Epic 9): every retired skill-manager
        // command is ABSENT at the TOP LEVEL, so `hekma agent list`/`show` is the one
        // canonical way to list/show the Fleet. These are top-level lookups on
        // `Cli::command()`, which only sees `agent` + `self-update`; `list`/`show`/
        // `remove` remain valid `hekma agent` SUBcommands, so this MUST stay a
        // top-level check — never a recursive/agent-tree search (that would
        // false-fail against the live `hekma agent list`/`show`/`remove`).
        for retired in [
            "init",
            "install",
            "search",
            "upgrade",
            "publish",
            "list",
            "show",
            "doctor",
            "uninstall",
            "remove",
        ] {
            assert!(
                cmd.find_subcommand(retired).is_none(),
                "retired command `{retired}` must not exist at the top level",
            );
        }
    }

    #[test]
    fn test_cli_identity_is_agent_framed_not_skills() {
        // Epic 9 rebrand: the top-level clap identity and the crate description
        // present Hekma as the agent runner, with no skills-package-manager
        // framing (mirrors `test_cli_help_includes_license_and_repository`).
        let about = Cli::command()
            .get_about()
            .expect("about should be set")
            .to_string()
            .to_lowercase();
        assert!(
            !about.contains("agentic")
                && !about.contains("skills package manager")
                && !about.contains("package manager"),
            "about still carries skills/package-manager framing: {about}",
        );
        assert!(
            about.contains("agent"),
            "about is not agent-framed: {about}"
        );

        let description = env!("CARGO_PKG_DESCRIPTION").to_lowercase();
        assert!(
            !description.contains("skills package manager")
                && !description.contains("package manager"),
            "description still carries package-manager framing: {description}",
        );
        assert!(
            description.contains("agent"),
            "description is not agent-framed: {description}",
        );
    }

    #[test]
    fn test_agent_subcommands_exist() {
        let cmd = Cli::command();
        let agent = cmd
            .find_subcommand("agent")
            .expect("agent subcommand should exist");
        assert!(agent.get_subcommands().any(|c| c.get_name() == "register"));
        assert!(agent.get_subcommands().any(|c| c.get_name() == "remove"));
        assert!(agent.get_subcommands().any(|c| c.get_name() == "start"));
        assert!(agent.get_subcommands().any(|c| c.get_name() == "stop"));
        assert!(agent.get_subcommands().any(|c| c.get_name() == "pause"));
        assert!(agent.get_subcommands().any(|c| c.get_name() == "resume"));
        assert!(agent.get_subcommands().any(|c| c.get_name() == "send"));
        assert!(agent.get_subcommands().any(|c| c.get_name() == "logs"));
        assert!(agent.get_subcommands().any(|c| c.get_name() == "usage"));
        assert!(agent.get_subcommands().any(|c| c.get_name() == "list"));
        assert!(agent.get_subcommands().any(|c| c.get_name() == "show"));
        assert!(agent.get_subcommands().any(|c| c.get_name() == "config"));
        assert!(agent.get_subcommands().any(|c| c.get_name() == "memory"));
        assert!(agent
            .get_subcommands()
            .any(|c| c.get_name() == "governance-vectors"));
    }

    #[test]
    fn test_agent_config_parse() {
        // Story 2-1: `config set <name> <key> <value>` and
        // `config get <name> [key]` parse (a nested subcommand).
        assert!(
            Cli::try_parse_from(["hekma", "agent", "config", "set", "demo", "model", "gpt-4"])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["hekma", "agent", "config", "get", "demo"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "config", "get", "demo", "model"]).is_ok());
        // Story 2-3: `config get` accepts `--json` (whole config or single key).
        assert!(Cli::try_parse_from(["hekma", "agent", "config", "get", "demo", "--json"]).is_ok());
        assert!(Cli::try_parse_from([
            "hekma", "agent", "config", "get", "demo", "model", "--json"
        ])
        .is_ok());
        // set requires all three positional args.
        assert!(Cli::try_parse_from(["hekma", "agent", "config", "set", "demo", "model"]).is_err());
        assert!(Cli::try_parse_from(["hekma", "agent", "config", "set", "demo"]).is_err());
        // get requires at least a name.
        assert!(Cli::try_parse_from(["hekma", "agent", "config", "get"]).is_err());
        // config requires a subcommand.
        assert!(Cli::try_parse_from(["hekma", "agent", "config"]).is_err());
    }

    #[test]
    fn test_agent_memory_parse() {
        // Story 5-1: `memory attach <name> --kind <kind>` and `memory detach
        // <name>` parse (a nested subcommand, mirroring `config`).
        assert!(Cli::try_parse_from([
            "hekma",
            "agent",
            "memory",
            "attach",
            "demo",
            "--kind",
            "filesystem"
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "memory", "detach", "demo"]).is_ok());
        // attach requires --kind.
        assert!(Cli::try_parse_from(["hekma", "agent", "memory", "attach", "demo"]).is_err());
        // attach requires a name.
        assert!(Cli::try_parse_from([
            "hekma",
            "agent",
            "memory",
            "attach",
            "--kind",
            "filesystem"
        ])
        .is_err());
        // detach requires a name.
        assert!(Cli::try_parse_from(["hekma", "agent", "memory", "detach"]).is_err());
        // memory requires a subcommand.
        assert!(Cli::try_parse_from(["hekma", "agent", "memory"]).is_err());
        // The kind value is NOT validated by clap (an unknown token is a runtime
        // usage diagnostic, exit 2 — pinned in the agent_cli integration tests).
        assert!(Cli::try_parse_from([
            "hekma", "agent", "memory", "attach", "demo", "--kind", "bogus"
        ])
        .is_ok());
    }

    #[test]
    fn test_agent_governance_vectors_parse() {
        // Story 19-1: `governance-vectors` and `governance-vectors --json`
        // parse; the verb takes no positional arguments.
        assert!(Cli::try_parse_from(["hekma", "agent", "governance-vectors"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "governance-vectors", "--json"]).is_ok());
        // No positional arguments exist.
        assert!(
            Cli::try_parse_from(["hekma", "agent", "governance-vectors", "extra"]).is_err(),
            "governance-vectors takes no positional argument"
        );
    }

    #[test]
    fn test_agent_start_stop_parse() {
        // `start <name>` and `stop <name> [--timeout <secs>]` parse.
        assert!(Cli::try_parse_from(["hekma", "agent", "start", "svc"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "stop", "svc"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "stop", "svc", "--timeout", "10"]).is_ok());
        // start requires a name.
        assert!(Cli::try_parse_from(["hekma", "agent", "start"]).is_err());
        // --timeout must be a number.
        assert!(
            Cli::try_parse_from(["hekma", "agent", "stop", "svc", "--timeout", "abc"]).is_err()
        );
    }

    #[test]
    fn test_agent_list_and_show_accept_json_flag() {
        // Story 1-7: `--json` is ADDED to the Agent `list`/`show` subcommands
        // (they took none before). Bare forms and the `--json` forms both parse.
        assert!(Cli::try_parse_from(["hekma", "agent", "list"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "list", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "show", "svc"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "show", "svc", "--json"]).is_ok());
        // `show --json` still requires a name.
        assert!(Cli::try_parse_from(["hekma", "agent", "show", "--json"]).is_err());
    }

    #[test]
    fn test_agent_pause_resume_parse() {
        // `pause <name>` and `resume <name>` parse (story 1-5).
        assert!(Cli::try_parse_from(["hekma", "agent", "pause", "svc"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "resume", "svc"]).is_ok());
        // Both require a name.
        assert!(Cli::try_parse_from(["hekma", "agent", "pause"]).is_err());
        assert!(Cli::try_parse_from(["hekma", "agent", "resume"]).is_err());
    }

    #[test]
    fn test_agent_send_parse() {
        // `send <name> <text>` parses (story 4-1); a multi-word text is a
        // single quoted positional, mirroring `config set`'s per-value
        // positional convention.
        assert!(Cli::try_parse_from(["hekma", "agent", "send", "svc", "hi"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "send", "svc", "hello there"]).is_ok());
        // Missing text, or missing both, is a clap error.
        assert!(Cli::try_parse_from(["hekma", "agent", "send", "svc"]).is_err());
        assert!(Cli::try_parse_from(["hekma", "agent", "send"]).is_err());
    }

    #[test]
    fn test_agent_send_text_is_hyphen_safe_and_help_is_not_intercepted() {
        // M1 fix (review of #79): a `text` value starting with a hyphen must
        // parse as a LITERAL value, not be rejected as an unrecognized flag
        // and not be silently swallowed as this CLI's own `--help`/`-h`.
        let parsed = Cli::try_parse_from(["hekma", "agent", "send", "x", "-5 degrees"])
            .expect("a hyphen-leading text value must parse, not error");
        let Some(Commands::Agent {
            command: AgentCommands::Send { name, text },
        }) = parsed.command
        else {
            panic!("expected Agent(Send)");
        };
        assert_eq!(name, "x");
        assert_eq!(text, "-5 degrees");

        // The specific silent-success bug: `text == "--help"` used to be
        // intercepted as a request for CLI help (Err(DisplayHelp), which
        // `Parser::parse()` renders by printing help and exiting 0 — NOTHING
        // sent, yet a caller checking only the exit code believed it
        // succeeded). It must now parse OK with the literal value retained.
        let parsed = Cli::try_parse_from(["hekma", "agent", "send", "x", "--help"])
            .expect("--help as a text value must not be intercepted as CLI help");
        let Some(Commands::Agent {
            command: AgentCommands::Send { name, text },
        }) = parsed.command
        else {
            panic!("expected Agent(Send)");
        };
        assert_eq!(name, "x");
        assert_eq!(text, "--help");

        // `-h` (the short form) must be treated identically.
        let parsed = Cli::try_parse_from(["hekma", "agent", "send", "x", "-h"])
            .expect("-h as a text value must not be intercepted as CLI help");
        let Some(Commands::Agent {
            command: AgentCommands::Send { text, .. },
        }) = parsed.command
        else {
            panic!("expected Agent(Send)");
        };
        assert_eq!(text, "-h");
    }

    #[test]
    fn test_agent_logs_parse() {
        // `logs <name>` and `logs <name> --follow`/`-f` parse (story 4-2), plus
        // `logs <name> --json` and the `--follow --json` combination (story 4-3).
        assert!(Cli::try_parse_from(["hekma", "agent", "logs", "svc"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "logs", "svc", "--follow"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "logs", "svc", "-f"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "logs", "svc", "--json"]).is_ok());
        assert!(
            Cli::try_parse_from(["hekma", "agent", "logs", "svc", "--follow", "--json"]).is_ok()
        );
        // Missing name is a clap error.
        assert!(Cli::try_parse_from(["hekma", "agent", "logs"]).is_err());

        // The bare form defaults follow AND json to false; --follow/-f sets follow,
        // --json sets json.
        let parsed = Cli::try_parse_from(["hekma", "agent", "logs", "svc"]).unwrap();
        let Some(Commands::Agent {
            command: AgentCommands::Logs { name, follow, json },
        }) = parsed.command
        else {
            panic!("expected Agent(Logs)");
        };
        assert_eq!(name, "svc");
        assert!(!follow);
        assert!(!json);

        let parsed = Cli::try_parse_from(["hekma", "agent", "logs", "svc", "-f"]).unwrap();
        let Some(Commands::Agent {
            command: AgentCommands::Logs { follow, json, .. },
        }) = parsed.command
        else {
            panic!("expected Agent(Logs)");
        };
        assert!(follow);
        assert!(!json);

        let parsed = Cli::try_parse_from(["hekma", "agent", "logs", "svc", "--json"]).unwrap();
        let Some(Commands::Agent {
            command: AgentCommands::Logs { json, .. },
        }) = parsed.command
        else {
            panic!("expected Agent(Logs)");
        };
        assert!(json);
    }

    #[test]
    fn test_agent_usage_parse() {
        // Story 4-3: `usage` (Fleet-wide), `usage <name>`, and either with `--json`
        // all parse — the optional positional name mirrors `show`/`list`'s duality.
        assert!(Cli::try_parse_from(["hekma", "agent", "usage"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "usage", "svc"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "usage", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["hekma", "agent", "usage", "svc", "--json"]).is_ok());

        // No name → None (the Fleet-wide form); a name → Some.
        let parsed = Cli::try_parse_from(["hekma", "agent", "usage", "--json"]).unwrap();
        let Some(Commands::Agent {
            command: AgentCommands::Usage { name, json },
        }) = parsed.command
        else {
            panic!("expected Agent(Usage)");
        };
        assert_eq!(name, None);
        assert!(json);

        let parsed = Cli::try_parse_from(["hekma", "agent", "usage", "svc"]).unwrap();
        let Some(Commands::Agent {
            command: AgentCommands::Usage { name, json },
        }) = parsed.command
        else {
            panic!("expected Agent(Usage)");
        };
        assert_eq!(name.as_deref(), Some("svc"));
        assert!(!json);
    }

    #[test]
    fn test_agent_register_requires_kind_or_manifest() {
        // Neither flag → clap error (required_unless_present).
        assert!(Cli::try_parse_from(["hekma", "agent", "register", "demo"]).is_err());
        // --kind alone parses.
        assert!(
            Cli::try_parse_from(["hekma", "agent", "register", "demo", "--kind", "mock"]).is_ok()
        );
        // --manifest alone parses.
        assert!(
            Cli::try_parse_from(["hekma", "agent", "register", "demo", "--manifest", "./a"])
                .is_ok()
        );
        // Both together → conflict error.
        assert!(Cli::try_parse_from([
            "hekma",
            "agent",
            "register",
            "demo",
            "--kind",
            "mock",
            "--manifest",
            "./a"
        ])
        .is_err());
    }

    #[test]
    fn test_cli_help_includes_license_and_repository() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("License: Apache License Version 2.0"));
        assert!(help.contains("Repository: https://github.com/Ktesio/hekma"));
    }

    #[test]
    fn test_help_footer_license_title_matches_shipped_license() {
        // Drift guard: HELP_FOOTER prints the license title as a literal
        // (Cargo exposes no CARGO_PKG_LICENSE under `license-file`). If the
        // shipped LICENSE is ever retitled, this fails CI instead of
        // `hekma --help` silently advertising a license the repo no longer
        // ships. The title must appear in the binding text below the
        // separator, not just the non-binding preface.
        let license = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../LICENSE"));
        // The license title the footer claims (first line, after the
        // prefix) — asserted against the SHIPPED LICENSE below, so a
        // LICENSE retitle fails CI instead of the footer advertising
        // terms the file no longer carries. (The footer's parenthetical
        // SPDX suffix is cosmetic and not part of the match.)
        let footer_title = HELP_FOOTER
            .strip_prefix("License: ")
            .and_then(|rest| rest.split('\n').next())
            .and_then(|rest| rest.split(" (").next())
            .expect("HELP_FOOTER must start with the license title");
        let title = footer_title;
        // Whitespace-normalized matching: the canonical Apache-2.0 text
        // wraps its title across lines ("Apache License\n Version 2.0"),
        // so the title only matches after normalization. (The footer's
        // parenthetical SPDX suffix is cosmetic and not part of the match.)
        let normalize = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        let binding = license;
        assert!(
            normalize(binding).contains(&normalize(title)),
            "LICENSE's terms must contain the printed license title {title:?}"
        );
    }

    #[test]
    fn test_cli_without_subcommand_is_allowed_for_help_display() {
        let cli = Cli::try_parse_from(["hekma"]).expect("bare hekma should parse");
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_subcommand_help_includes_details_and_examples() {
        for (command, detail) in [
            ("self-update", "Updates the hekma binary"),
            ("agent", "Manages Agent Instances"),
        ] {
            let mut cmd = Cli::command();
            let help = cmd
                .find_subcommand_mut(command)
                .expect("subcommand should exist")
                .render_help()
                .to_string();
            assert!(help.contains(detail), "{} help missing detail", command);
            assert!(help.contains("Example"), "{} help missing example", command);
        }
    }

    /// Render the short and long help of `cmd` and every nested subcommand.
    fn collect_help(cmd: &mut clap::Command, path: &str, out: &mut Vec<(String, String)>) {
        out.push((path.to_string(), cmd.render_help().to_string()));
        out.push((path.to_string(), cmd.render_long_help().to_string()));
        for sub in cmd.get_subcommands_mut() {
            let sub_path = format!("{path} {}", sub.get_name());
            collect_help(sub, &sub_path, out);
        }
    }

    #[test]
    fn test_help_text_carries_no_retired_names_or_planning_ids() {
        // Help text is read by users: no retired `kt` command, no internal
        // planning ids or review notes, and no old product name for Hekma.
        let mut root = Cli::command();
        root.build();
        let mut pages = Vec::new();
        collect_help(&mut root, "hekma", &mut pages);
        assert!(
            pages.iter().any(|(path, _)| path == "hekma agent send"),
            "the walk must reach nested subcommands"
        );
        let banned = [
            "kt agent",
            "`kt",
            "FR-",
            "NFR-",
            "AD-1",
            "AD-2",
            "AD-9",
            "AI-",
            "DC-",
            "story ",
            "Story ",
            "Epic",
            "ratified",
            "review finding",
            "review of #",
            "M1 fix",
            "honest seed",
            "its park",
            "docs/control-api.md",
        ];
        for (path, help) in &pages {
            for term in banned {
                assert!(
                    !help.contains(term),
                    "`{path}` help contains {term:?}:\n{help}"
                );
            }
            // The repository URL keeps the GitHub org name; any other
            // "Ktesio" would be the old product name.
            assert!(
                !help.replace("github.com/Ktesio/", "").contains("Ktesio"),
                "`{path}` help names Ktesio:\n{help}"
            );
        }
    }

    #[test]
    fn test_self_update_skips_passive_update_check() {
        let cli = Cli::try_parse_from(["hekma", "self-update"]).expect("self-update should parse");

        assert!(!should_check_for_updates(&cli.command));
    }

    #[test]
    fn test_serve_skips_passive_update_check() {
        // Review finding 11: `serve` is the OTHER passive surface — a
        // long-running host is not a point-in-time command, and its stderr
        // stream carries host diagnostics, not update notices. Pinned as the
        // sibling of the self-update pin.
        let cli = Cli::try_parse_from(["hekma", "serve"]).expect("serve should parse");

        assert!(!should_check_for_updates(&cli.command));
    }

    #[test]
    fn test_serve_control_api_flag_pair_lands_in_the_right_fields() {
        // Review item 24(a): the Control-API flags are pinned at BOTH
        // binary surfaces. This pin holds the clap parse — the pair's values
        // land in the RIGHT fields, and each half ALONE is refused. The
        // real-binary wiring leg (the secret resolved from the secrets file,
        // the bearer answered over the wire) lives in
        // `crates/hekma/tests/serve_cli.rs`.
        let cli = Cli::try_parse_from([
            "hekma",
            "serve",
            "--listen",
            "127.0.0.1:8477",
            "--bearer-secret",
            "CONTROL_BEARER",
        ])
        .expect("the flag pair parses");
        let Some(Commands::Serve {
            listen,
            bearer_secret,
            ..
        }) = &cli.command
        else {
            panic!("expected the serve subcommand");
        };
        assert_eq!(
            *listen,
            Some("127.0.0.1:8477".parse().unwrap()),
            "the listen address lands in the listen field"
        );
        assert_eq!(
            bearer_secret.as_deref(),
            Some("CONTROL_BEARER"),
            "the bearer NAME lands in the bearer_secret field"
        );

        // Each half ALONE is refused (clap's `requires` — the API never
        // serves unauthenticated, and a bearer without a listener is inert).
        match Cli::try_parse_from(["hekma", "serve", "--listen", "127.0.0.1:8477"]) {
            Ok(_) => panic!("--listen without --bearer-secret must be refused"),
            Err(err) => assert!(
                err.use_stderr(),
                "a half-pair is a usage error, not a silent default"
            ),
        }
        if Cli::try_parse_from(["hekma", "serve", "--bearer-secret", "CONTROL_BEARER"]).is_ok() {
            panic!("--bearer-secret without --listen must be refused");
        }
    }

    #[test]
    fn test_serve_log_capture_flag_parses_and_lands_in_the_right_field() {
        // Story 17-4 (the 17-3 parse-pin lesson): `--log-capture` is pinned
        // at BOTH binary surfaces. This pin holds the clap parse — every
        // accepted value lands in the RIGHT field, the default is `full`,
        // and an invalid value is a PARSE refusal (never a silent default to
        // a different mode).
        let default = Cli::try_parse_from(["hekma", "serve"]).expect("bare serve parses");
        let Some(Commands::Serve { log_capture, .. }) = &default.command else {
            panic!("expected the serve subcommand");
        };
        assert_eq!(
            *log_capture, None,
            "no flag parses as None — the default lives ONLY in serve::run \
             (review finding 19: no clap default_value duplicates it)"
        );

        for (text, expected) in [
            ("full", hekma_engine::LogCaptureMode::Full),
            ("metadata", hekma_engine::LogCaptureMode::Metadata),
            ("off", hekma_engine::LogCaptureMode::Off),
        ] {
            let cli = Cli::try_parse_from(["hekma", "serve", "--log-capture", text])
                .unwrap_or_else(|err| panic!("--log-capture {text} must parse: {err}"));
            let Some(Commands::Serve { log_capture, .. }) = &cli.command else {
                panic!("expected the serve subcommand");
            };
            assert_eq!(
                *log_capture,
                Some(expected),
                "--log-capture {text} lands in the log_capture field"
            );
        }

        match Cli::try_parse_from(["hekma", "serve", "--log-capture", "sometimes"]) {
            Ok(_) => panic!("an invalid mode must be a parse refusal"),
            Err(err) => assert!(
                err.use_stderr(),
                "an invalid mode is a usage error naming the accepted values"
            ),
        }
    }
}
