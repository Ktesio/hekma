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
    "License: Ktesio Noncommercial-Attribution License 1.0.0",
    "\nRepository: ",
    env!("CARGO_PKG_REPOSITORY")
);

const SELF_UPDATE_AFTER_HELP: &str = "\
Details:
  Updates the kt binary using the current install channel. Homebrew installs run
  brew upgrade, Cargo installs run cargo install --force, and manual binary
  installs download and verify the latest GitHub Release archive.

Example:
  kt self-update";

const AGENT_AFTER_HELP: &str = "\
Details:
  An agent is a third-party program you run — a personal agent such as Hermes
  Agent or OpenClaw, or a coding agent such as OpenCode or GitHub Copilot CLI —
  that kt launches, supervises, meters, and budgets as a process.
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
  accept --json for a machine-readable document (usage is now real token totals
  from the Usage Ledger, while budget/cap stays the honest seed: '—' in the table,
  null in JSON). config set writes a key to the Agent Instance layer (validated at write
  time — an unknown key outside the agent.* pass-through namespace is rejected
  with the nearest valid key suggested, and nothing is persisted); config get
  prints the effective (resolved) config, where a key set at the instance layer
  overrides the same key at the kind/engine-default layer, every time (FR-11);
  each value names its source layer (a Source column, or a source field with
  --json), and starting an instance persists an effective-config snapshot in the
  Agent Home. A secret:NAME value is resolved from the environment or the engine
  secrets file at start and delivered to the agent, but is MASKED in config get,
  the snapshot, logs, and events (FR-14); config get --reveal is the sole way to
  print it unmasked. Removing a running instance requires --force.

Examples:
  kt agent register demo --kind mock
  kt agent register my-agent --manifest ./my-agent
  kt agent start my-agent
  kt agent pause my-agent
  kt agent resume my-agent
  kt agent stop my-agent --timeout 10
  kt agent show demo
  kt agent list
  kt agent list --json
  kt agent config set demo model gpt-4
  kt agent config set demo agent.api_key secret:OPENAI_KEY
  kt agent config get demo
  kt agent config get demo model
  kt agent config get demo --json
  kt agent config get demo --reveal
  kt agent remove demo --delete";

#[derive(Parser)]
#[command(
    name = "kt",
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
    /// Update the kt binary
    #[command(
        name = "self-update",
        about = "Update the kt binary",
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
    /// Send text input to a running Agent Instance's native input channel
    ///
    /// M1 fix (review of #79): `--help`/`-h` are DISABLED on this subcommand
    /// specifically, and `text` accepts leading-hyphen values, so a `text`
    /// payload that happens to look like a flag (`"-5 degrees"`, `"--help"`)
    /// is delivered LITERALLY rather than either failing to parse or being
    /// silently intercepted as this CLI's own help (which used to print
    /// help and exit 0 without sending anything — a caller checking only
    /// the exit code would wrongly believe the send succeeded). A caller
    /// that genuinely wants help for `send` gets it from `kt agent --help`
    /// or `kt agent send` with a missing argument's error text.
    #[command(disable_help_flag = true)]
    Send {
        /// Name of the Agent Instance to send input to
        name: String,
        /// The text to send (a trailing newline is appended if absent).
        /// Accepts a value starting with `-`/`--` (e.g. `"-5 degrees"`)
        /// literally, instead of clap trying to parse it as a flag.
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
        /// Emit each captured line as newline-delimited JSON (one self-versioned
        /// engine `LogLine` per stdout line, NDJSON — AD-14); identical shape for
        /// one-shot and `--follow`. Notices/diagnostics stay on stderr.
        #[arg(long)]
        json: bool,
    },
    /// Report Usage Ledger token totals (and derived dollar cost) for one
    /// instance, or Fleet-wide totals when no name is given (FR-22)
    Usage {
        /// Optional Agent Instance name; omitted reports the Fleet-wide totals
        name: Option<String>,
        /// Emit the usage snapshot as a machine-readable JSON document (AD-14)
        #[arg(long)]
        json: bool,
    },
    /// List every Agent Instance in the Fleet
    List {
        /// Emit the Fleet as a machine-readable JSON document (FR-4 / AD-14)
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
    /// Get or set an Agent Instance's unified config (layered, FR-11)
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Attach or detach an Agent Instance's Memory Backing (FR-15)
    Memory {
        #[command(subcommand)]
        command: MemoryCommands,
    },
}

#[derive(Subcommand)]
enum MemoryCommands {
    /// Attach a Memory Backing to an Agent Instance (requires a terminal state —
    /// no hot-swap). `filesystem` creates the managed directory inside the Agent
    /// Home; `native` records the delegation (Ktesio guarantees only Agent Home
    /// persistence) and creates nothing.
    Attach {
        /// Name of the Agent Instance
        name: String,
        /// The Memory Backing kind to attach (accepted: filesystem, native)
        #[arg(long)]
        kind: String,
        /// Emit the attachment as a machine-readable JSON document (AD-14;
        /// story 6-6's deferred wire surface, frozen at the contract v1 freeze)
        #[arg(long)]
        json: bool,
    },
    /// Detach an Agent Instance's Memory Backing (metadata only: the managed
    /// directory and its contents remain on disk; requires a terminal state)
    Detach {
        /// Name of the Agent Instance
        name: String,
        /// Emit the detachment as a machine-readable JSON document (AD-14;
        /// story 6-6's deferred wire surface, frozen at the contract v1 freeze)
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
        /// Value to set (stored verbatim; a `secret:NAME` reference is resolved +
        /// masked at start/read, FR-14 — the reference is what is stored here).
        /// Accepts a value starting with `-`/`--` (e.g. `-x`) LITERALLY — clap
        /// does not intercept it as a flag (story 11-2, AI-26; mirrors `send`'s
        /// `text`), so the `--` separator is optional rather than mandatory
        /// (it still works for callers who prefer it).
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Get an Agent Instance's effective (resolved) config value(s) with per-value source
    Get {
        /// Name of the Agent Instance
        name: String,
        /// Optional config key; omitted prints the whole effective config
        key: Option<String>,
        /// Emit the effective config (value + source layer per leaf) as JSON (FR-13)
        #[arg(long)]
        json: bool,
        /// Reveal secret values unmasked (the SOLE explicit acknowledgment; FR-14).
        /// Without it, `secret:` values are masked in both the table and --json.
        /// Never un-masks the persisted snapshot, logs, or events. Re-resolves
        /// secrets LIVE (env, then the secrets file) at read time, so a revealed
        /// value may differ from what a running instance resolved at its start.
        #[arg(long)]
        reveal: bool,
    },
}

#[cfg(not(tarpaulin_include))]
fn main() {
    if let Err(err) = run_cli() {
        // Classify the boxed diagnostic into the documented, stable numeric exit
        // code (story 4-3, FR-26 / PRD §7) BEFORE it is consumed by `ui::error`.
        // clap's own usage/parse errors already exit `2` and `--help`/`--version`
        // exit `0` from inside `Cli::parse()`, so they never reach here; this maps
        // only the runtime diagnostics, with any unmapped error falling to `1`
        // (preserving the pre-4-3 behavior). See `exit_code::classify`.
        let code = exit_code::classify(err.as_ref());
        ui::error(err);
        std::process::exit(code.code());
    }
}

#[cfg(not(tarpaulin_include))]
fn run_cli() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    if should_check_for_updates(&cli.command) {
        if let Some(notice) = update_check::maybe_notice() {
            ui::update_notice(&notice.current_version, &notice.latest_version);
        }
    }

    match cli.command {
        Some(Commands::SelfUpdate) => cli::self_update::run(),
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
            AgentCommands::Start { name } => cli::agent::start(&name),
            AgentCommands::Stop { name, timeout } => cli::agent::stop(&name, timeout),
            AgentCommands::Pause { name } => cli::agent::pause(&name),
            AgentCommands::Resume { name } => cli::agent::resume(&name),
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
        },
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

fn should_check_for_updates(command: &Option<Commands>) -> bool {
    matches!(command, Some(command) if !matches!(command, Commands::SelfUpdate))
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
        // command is ABSENT at the TOP LEVEL, so `kt agent list`/`show` is the one
        // canonical way to list/show the Fleet. These are top-level lookups on
        // `Cli::command()`, which only sees `agent` + `self-update`; `list`/`show`/
        // `remove` remain valid `kt agent` SUBcommands, so this MUST stay a
        // top-level check — never a recursive/agent-tree search (that would
        // false-fail against the live `kt agent list`/`show`/`remove`).
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
        // present Ktesio as the agent runner, with no skills-package-manager
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
    }

    #[test]
    fn test_agent_config_parse() {
        // Story 2-1: `config set <name> <key> <value>` and
        // `config get <name> [key]` parse (a nested subcommand).
        assert!(
            Cli::try_parse_from(["kt", "agent", "config", "set", "demo", "model", "gpt-4"]).is_ok()
        );
        assert!(Cli::try_parse_from(["kt", "agent", "config", "get", "demo"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "config", "get", "demo", "model"]).is_ok());
        // Story 2-3: `config get` accepts `--json` (whole config or single key).
        assert!(Cli::try_parse_from(["kt", "agent", "config", "get", "demo", "--json"]).is_ok());
        assert!(
            Cli::try_parse_from(["kt", "agent", "config", "get", "demo", "model", "--json"])
                .is_ok()
        );
        // set requires all three positional args.
        assert!(Cli::try_parse_from(["kt", "agent", "config", "set", "demo", "model"]).is_err());
        assert!(Cli::try_parse_from(["kt", "agent", "config", "set", "demo"]).is_err());
        // get requires at least a name.
        assert!(Cli::try_parse_from(["kt", "agent", "config", "get"]).is_err());
        // config requires a subcommand.
        assert!(Cli::try_parse_from(["kt", "agent", "config"]).is_err());
    }

    #[test]
    fn test_agent_memory_parse() {
        // Story 5-1: `memory attach <name> --kind <kind>` and `memory detach
        // <name>` parse (a nested subcommand, mirroring `config`).
        assert!(Cli::try_parse_from([
            "kt",
            "agent",
            "memory",
            "attach",
            "demo",
            "--kind",
            "filesystem"
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "memory", "detach", "demo"]).is_ok());
        // attach requires --kind.
        assert!(Cli::try_parse_from(["kt", "agent", "memory", "attach", "demo"]).is_err());
        // attach requires a name.
        assert!(
            Cli::try_parse_from(["kt", "agent", "memory", "attach", "--kind", "filesystem"])
                .is_err()
        );
        // detach requires a name.
        assert!(Cli::try_parse_from(["kt", "agent", "memory", "detach"]).is_err());
        // memory requires a subcommand.
        assert!(Cli::try_parse_from(["kt", "agent", "memory"]).is_err());
        // The kind value is NOT validated by clap (an unknown token is a runtime
        // usage diagnostic, exit 2 — pinned in the agent_cli integration tests).
        assert!(Cli::try_parse_from([
            "kt", "agent", "memory", "attach", "demo", "--kind", "bogus"
        ])
        .is_ok());
    }

    #[test]
    fn test_agent_start_stop_parse() {
        // `start <name>` and `stop <name> [--timeout <secs>]` parse.
        assert!(Cli::try_parse_from(["kt", "agent", "start", "svc"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "stop", "svc"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "stop", "svc", "--timeout", "10"]).is_ok());
        // start requires a name.
        assert!(Cli::try_parse_from(["kt", "agent", "start"]).is_err());
        // --timeout must be a number.
        assert!(Cli::try_parse_from(["kt", "agent", "stop", "svc", "--timeout", "abc"]).is_err());
    }

    #[test]
    fn test_agent_list_and_show_accept_json_flag() {
        // Story 1-7: `--json` is ADDED to the Agent `list`/`show` subcommands
        // (they took none before). Bare forms and the `--json` forms both parse.
        assert!(Cli::try_parse_from(["kt", "agent", "list"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "list", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "show", "svc"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "show", "svc", "--json"]).is_ok());
        // `show --json` still requires a name.
        assert!(Cli::try_parse_from(["kt", "agent", "show", "--json"]).is_err());
    }

    #[test]
    fn test_agent_pause_resume_parse() {
        // `pause <name>` and `resume <name>` parse (story 1-5).
        assert!(Cli::try_parse_from(["kt", "agent", "pause", "svc"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "resume", "svc"]).is_ok());
        // Both require a name.
        assert!(Cli::try_parse_from(["kt", "agent", "pause"]).is_err());
        assert!(Cli::try_parse_from(["kt", "agent", "resume"]).is_err());
    }

    #[test]
    fn test_agent_send_parse() {
        // `send <name> <text>` parses (story 4-1); a multi-word text is a
        // single quoted positional, mirroring `config set`'s per-value
        // positional convention.
        assert!(Cli::try_parse_from(["kt", "agent", "send", "svc", "hi"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "send", "svc", "hello there"]).is_ok());
        // Missing text, or missing both, is a clap error.
        assert!(Cli::try_parse_from(["kt", "agent", "send", "svc"]).is_err());
        assert!(Cli::try_parse_from(["kt", "agent", "send"]).is_err());
    }

    #[test]
    fn test_agent_send_text_is_hyphen_safe_and_help_is_not_intercepted() {
        // M1 fix (review of #79): a `text` value starting with a hyphen must
        // parse as a LITERAL value, not be rejected as an unrecognized flag
        // and not be silently swallowed as this CLI's own `--help`/`-h`.
        let parsed = Cli::try_parse_from(["kt", "agent", "send", "x", "-5 degrees"])
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
        let parsed = Cli::try_parse_from(["kt", "agent", "send", "x", "--help"])
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
        let parsed = Cli::try_parse_from(["kt", "agent", "send", "x", "-h"])
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
        assert!(Cli::try_parse_from(["kt", "agent", "logs", "svc"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "logs", "svc", "--follow"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "logs", "svc", "-f"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "logs", "svc", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "logs", "svc", "--follow", "--json"]).is_ok());
        // Missing name is a clap error.
        assert!(Cli::try_parse_from(["kt", "agent", "logs"]).is_err());

        // The bare form defaults follow AND json to false; --follow/-f sets follow,
        // --json sets json.
        let parsed = Cli::try_parse_from(["kt", "agent", "logs", "svc"]).unwrap();
        let Some(Commands::Agent {
            command: AgentCommands::Logs { name, follow, json },
        }) = parsed.command
        else {
            panic!("expected Agent(Logs)");
        };
        assert_eq!(name, "svc");
        assert!(!follow);
        assert!(!json);

        let parsed = Cli::try_parse_from(["kt", "agent", "logs", "svc", "-f"]).unwrap();
        let Some(Commands::Agent {
            command: AgentCommands::Logs { follow, json, .. },
        }) = parsed.command
        else {
            panic!("expected Agent(Logs)");
        };
        assert!(follow);
        assert!(!json);

        let parsed = Cli::try_parse_from(["kt", "agent", "logs", "svc", "--json"]).unwrap();
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
        assert!(Cli::try_parse_from(["kt", "agent", "usage"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "usage", "svc"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "usage", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["kt", "agent", "usage", "svc", "--json"]).is_ok());

        // No name → None (the Fleet-wide form); a name → Some.
        let parsed = Cli::try_parse_from(["kt", "agent", "usage", "--json"]).unwrap();
        let Some(Commands::Agent {
            command: AgentCommands::Usage { name, json },
        }) = parsed.command
        else {
            panic!("expected Agent(Usage)");
        };
        assert_eq!(name, None);
        assert!(json);

        let parsed = Cli::try_parse_from(["kt", "agent", "usage", "svc"]).unwrap();
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
        assert!(Cli::try_parse_from(["kt", "agent", "register", "demo"]).is_err());
        // --kind alone parses.
        assert!(Cli::try_parse_from(["kt", "agent", "register", "demo", "--kind", "mock"]).is_ok());
        // --manifest alone parses.
        assert!(
            Cli::try_parse_from(["kt", "agent", "register", "demo", "--manifest", "./a"]).is_ok()
        );
        // Both together → conflict error.
        assert!(Cli::try_parse_from([
            "kt",
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
        assert!(help.contains("License: Ktesio Noncommercial-Attribution License 1.0.0"));
        assert!(help.contains("Repository: https://github.com/iMagdy/ktesio"));
    }

    #[test]
    fn test_help_footer_license_title_matches_shipped_license() {
        // Drift guard: HELP_FOOTER prints the license title as a literal
        // (Cargo exposes no CARGO_PKG_LICENSE under `license-file`). If the
        // shipped LICENSE is ever retitled, this fails CI instead of
        // `kt --help` silently advertising a license the repo no longer
        // ships. The title must appear in the binding text below the
        // separator, not just the non-binding preface.
        let license = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../LICENSE"));
        let title = HELP_FOOTER
            .strip_prefix("License: ")
            .and_then(|rest| rest.split('\n').next())
            .expect("HELP_FOOTER must start with the license title");
        let binding = license
            .split_once("\n----")
            .map(|(_, rest)| rest)
            .unwrap_or(license);
        assert!(
            binding.contains(title),
            "LICENSE's binding terms must contain the printed license title {title:?}"
        );
    }

    #[test]
    fn test_cli_without_subcommand_is_allowed_for_help_display() {
        let cli = Cli::try_parse_from(["kt"]).expect("bare kt should parse");
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_subcommand_help_includes_details_and_examples() {
        for (command, detail) in [
            ("self-update", "Updates the kt binary"),
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

    #[test]
    fn test_self_update_skips_passive_update_check() {
        let cli = Cli::try_parse_from(["kt", "self-update"]).expect("self-update should parse");

        assert!(!should_check_for_updates(&cli.command));
    }
}
