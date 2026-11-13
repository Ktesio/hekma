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
  live agent at open, then waits. Every serve also binds a loopback local
  endpoint for the hekma CLI (recorded in <state-dir>/control.json); while a
  host serves the directory, every `hekma agent` write goes through it — and
  when none does, the first write starts one in the background. On SIGTERM (or Ctrl+C) every RUNNING agent
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
  hekma serve --otlp-endpoint http://127.0.0.1:4318 \\
      --otlp-headers authorization=secret:OTLP_AUTH

The Control API (--listen + --bearer-secret, both or neither) exposes the
unstable v0 HTTP/JSON surface — fleet, per-instance lifecycle/config/status,
and events (SSE + cursor pull) — guarded by the resolved bearer token. Beyond
loopback it requires TLS (--tls-cert + --tls-key); see
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

const RUN_AFTER_HELP: &str = "\
Details:
  A Run is one unit of agent work with a stable run id, a Control Depth,
  labels, budgets and a Run Status. Starting an agent creates its supervised
  Run; a Restart Policy relaunch keeps it. open starts a child Run under a
  Run (--parent <run-id>) or under an agent's open Run (--parent <agent>):
  a Host's unit of work inside that agent's process. The child's usage
  counts on every ancestor and every ancestor's ceilings apply; its own
  ceilings (--token-budget, --dollar-cap) take the warn Breach Action.
  --label key=value adds caller labels; --external-id makes a retried open
  return the same Run. close ends a child Run; show prints any Run.
  open --print-key also issues the child's Run Key and refresh credential
  and prints them once: they are not stored and cannot be shown again.
  keys lists a Run's credentials (token ids and states, never a key);
  revoke-keys revokes them, and by default every credential issued under
  them (--no-cascade keeps the child Runs' keys). Every verb accepts --json.

Examples:
  hekma run open --parent my-agent --label task=review --token-budget 20000
  hekma run show run_01JZ6Q9G3S8V2X7K4M1N5P0R2T
  hekma run close run_01JZ6Q9G3S8V2X7K4M1N5P0R2T --json
  hekma run keys run_01JZ6Q9G3S8V2X7K4M1N5P0R2T
  hekma run revoke-keys run_01JZ6Q9G3S8V2X7K4M1N5P0R2T";

const LABEL_AFTER_HELP: &str = "\
Details:
  A label set here is bound to the Run: it outranks the agent's labels
  configuration and any label the Run set about itself, and it stays with
  the Run across a crash relaunch (a new start opens a new Run without it).
  remove takes back a label set here; if the agent's configuration binds the
  same key, its value returns. A label that comes from the configuration is
  changed with hekma agent config set, and a label the Run set about itself
  cannot be removed here.
  The target is a run id, an agent (its open Run), or --selector
  key=value[,key=value...], which matches every open Run carrying all those
  labels. When a Selector matches more than one Run, the set is listed and
  you are asked to confirm (--yes skips the question; without a terminal the
  command is refused unless --yes is given); --dry-run shows each Run's change
  and writes nothing. Each Run reports its own result; the exit code is the
  first failure's (3 not found, 4 refused by the Run's state or labels, 2 an
  invalid request). A Selector may match at most 1000 Runs.

Examples:
  hekma label set my-agent team=core
  hekma label set run_01JZ6Q9G3S8V2X7K4M1N5P0R2T env=prod --json
  hekma label set --selector team=core,env=prod tier=gold --dry-run
  hekma label remove --selector team=core tier --yes";

const SET_VERB_AFTER_HELP: &str = "\
Details:
  The target is an agent, a run id, or --selector key=value[,key=value...],
  which matches every open Run carrying all those labels. On the Run that
  owns an agent's process the verb is the agent's own; a child Run is
  refused, naming its depth (its lifecycle acts on its model calls, which is
  not available yet). When a Selector matches more than one Run, the set is
  listed and you are asked to confirm (--yes skips the question; without a
  terminal the command is refused unless --yes is given); --dry-run shows
  each Run's outcome and changes nothing. Each Run reports its own result;
  the exit code is the first failure's (3 not found, 4 refused by the Run's
  state or depth, 2 an invalid request). A Selector may match at most 1000
  Runs. start and send take no Selector: a Selector matches open Runs, so
  there is nothing to start, and send carries text to one agent.

Examples:
  hekma agent stop my-agent
  hekma agent stop --selector team=core --dry-run
  hekma agent pause run_01JZ6Q9G3S8V2X7K4M1N5P0R2T
  hekma agent drain --selector team=core --yes --json
  hekma run close --selector task=review --yes";

const BUDGET_AFTER_HELP: &str = "\
Details:
  show prints a Run's limits: each ceiling with where it comes from (run,
  instance, policy:<name>, default), its usage, the headroom, the Breach
  Action and any latched breach. set writes limits on the Run itself: they
  outrank the agent's budget configuration and every Policy for that limit.
  A limit set here belongs to the Run: it ends with the Run, and a new start
  opens a new Run without it. For limits every future Run should carry, set
  the agent's budget.* configuration or a Policy. A ceiling raised above the
  usage re-arms its breach and lifts a matching block at once; one lowered to
  or below the usage breaches at once. Cumulative ceilings go on an agent's
  own Run only. The target is a run id, an agent (its open Run), or
  --selector key=value[,key=value...]; with a Selector, set confirms like
  every set-taking verb. show with a Selector, or with no target at all,
  lists the Runs as hekma ps --all does (every open Run and those ended in
  the last 7 days), paged with --limit and --cursor.

Examples:
  hekma budget show my-agent
  hekma budget show --selector team=core --limit 20
  hekma budget set my-agent --tokens-per-run 500000
  hekma budget set run_01JZ6Q9G3S8V2X7K4M1N5P0R2T --dollars-per-run 3.00 --breach-action pause
  hekma budget set --selector team=core --clear tokens-per-run --yes";
const PS_AFTER_HELP: &str = "\
Details:
  The Runs that need you, newest first. The default view lists a Run iff it
  carries an Anomaly (an ended Run only inside the 7-day ended-Run window)
  or a held, silent or stale status; healthy open Runs are counted in the
  footer, not listed. --all lists every Run inside the window. The CONTROL
  column shows the Control Depth (supervised (no wire) for an agent whose
  model calls skip the metering proxy); the RUN column shows the agent's
  name on process-owning rows and the run id on child Runs. ANOMALIES shows
  the breach Anomaly with each latched Breach Action (breach[block]) beside
  the others; LABELS marks caller labels. USAGE is the Run's subtree tokens
  and derived dollars (an estimate); HEADROOM is what remains against the
  Run's resolved ceilings, naming the Policy that set them and, on
  process-owning rows, the cumulative token headroom. Both dollar figures
  are estimates and appear only where a Rate prices them.

  --selector key=value[,key=value...] filters by label (bound or caller
  alike) in either view. --limit N pages (default 50, at most 1000; 0 means
  the maximum); when another page exists the footer prints the exact
  --cursor value that continues the listing. The table follows the
  terminal width; --json carries every figure whole. A read-only read: no
  host is started and nothing is written. Exit codes: 2 an invalid
  selector, cursor or limit; 1 a store failure; 0 otherwise — an empty
  listing included.

Examples:
  hekma ps
  hekma ps --selector team=core,env=prod
  hekma ps --all --limit 100
  hekma ps --cursor dnVuXzFfMDEx --json";

#[derive(Parser)]
#[command(
    name = "hekma",
    version,
    about = "The control plane for agent workloads — run, see and steer AI agents",
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
    /// Open, close and show Runs
    #[command(about = "Open, close and show Runs", after_help = RUN_AFTER_HELP)]
    Run {
        #[command(subcommand)]
        command: RunCommands,
    },
    /// Bind and unbind labels on Runs
    #[command(about = "Bind and unbind labels on Runs", after_help = LABEL_AFTER_HELP)]
    Label {
        #[command(subcommand)]
        command: LabelCommands,
    },
    /// Show and set the limits of Runs
    #[command(about = "Show and set the limits of Runs", after_help = BUDGET_AFTER_HELP)]
    Budget {
        #[command(subcommand)]
        command: BudgetCommands,
    },
    /// Set limits for the Runs a Selector matches
    #[command(about = "Set limits for the Runs a Selector matches")]
    Policy {
        #[command(subcommand)]
        command: PolicyCommands,
    },
    /// Manage the metering proxy's endpoint bindings
    #[command(about = "Manage the metering proxy's endpoint bindings")]
    Binding {
        #[command(subcommand)]
        command: BindingCommands,
    },
    /// Read the metering proxy's per-call records
    #[command(about = "Read the metering proxy's per-call records")]
    Calls {
        #[command(subcommand)]
        command: CallsCommands,
    },
    /// Hold a Run's model calls at the metering proxy
    #[command(about = "Hold a Run's model calls at the metering proxy")]
    Hold {
        /// A run id (or use --selector)
        target: Option<String>,
        /// Every open Run carrying all these labels (key=value[,key=value...])
        #[arg(long, value_name = "SELECTOR")]
        selector: Option<String>,
        /// Show what would change; change nothing
        #[arg(long)]
        dry_run: bool,
        /// Act on several Runs without asking
        #[arg(long)]
        yes: bool,
        /// Print the results as one JSON document
        #[arg(long)]
        json: bool,
    },
    /// Release a held Run's model calls
    #[command(about = "Release a held Run's model calls")]
    Release {
        /// A run id (or use --selector)
        target: Option<String>,
        /// Every open Run carrying all these labels (key=value[,key=value...])
        #[arg(long, value_name = "SELECTOR")]
        selector: Option<String>,
        /// Show what would change; change nothing
        #[arg(long)]
        dry_run: bool,
        /// Act on several Runs without asking
        #[arg(long)]
        yes: bool,
        /// Print the results as one JSON document
        #[arg(long)]
        json: bool,
    },
    /// Send a Run's model calls to another binding route
    #[command(about = "Send a Run's model calls to another binding route")]
    Reroute {
        /// A run id (or use --selector)
        target: Option<String>,
        /// Every open Run carrying all these labels (key=value[,key=value...])
        #[arg(long, value_name = "SELECTOR")]
        selector: Option<String>,
        /// The route to send the calls to (`<binding>/<route>`)
        #[arg(long, value_name = "BINDING/ROUTE")]
        via: Option<String>,
        /// Send the calls back to the Run's own upstream
        #[arg(long)]
        off: bool,
        /// Show what would change; change nothing
        #[arg(long)]
        dry_run: bool,
        /// Act on several Runs without asking
        #[arg(long)]
        yes: bool,
        /// Print the results as one JSON document
        #[arg(long)]
        json: bool,
    },
    /// Refuse a Run's model calls at the metering proxy
    #[command(about = "Refuse a Run's model calls at the metering proxy")]
    Reject {
        /// A run id (or use --selector)
        target: Option<String>,
        /// Every open Run carrying all these labels (key=value[,key=value...])
        #[arg(long, value_name = "SELECTOR")]
        selector: Option<String>,
        /// Show what would change; change nothing
        #[arg(long)]
        dry_run: bool,
        /// Act on several Runs without asking
        #[arg(long)]
        yes: bool,
        /// Print the results as one JSON document
        #[arg(long)]
        json: bool,
    },
    /// List the Runs that need you, newest first
    #[command(
        about = "List the Runs that need you, newest first",
        after_help = PS_AFTER_HELP
    )]
    Ps {
        /// Only Runs carrying all these labels (key=value[,key=value...])
        #[arg(long, value_name = "SELECTOR")]
        selector: Option<String>,
        /// List every Run inside the ended-Run window, not just the
        /// exceptions
        #[arg(long)]
        all: bool,
        /// Page size (default 50, at most 1000; 0 means the maximum)
        #[arg(long, value_name = "N")]
        limit: Option<u64>,
        /// Continue after a previous page's cursor
        #[arg(long, value_name = "CURSOR")]
        cursor: Option<String>,
        /// Print the listing as one JSON document
        #[arg(long)]
        json: bool,
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
        /// The shared metering proxy's loopback port (default 0 = the port
        /// recorded for this state directory, else an OS pick that is then
        /// recorded — one stable address across restarts). A port already in
        /// use is surfaced and engine-observed agents cannot start until it
        /// is free (never a silent hop).
        #[arg(long, value_name = "PORT", default_value_t = 0)]
        metering_listen: u16,
        /// Send neither x-hekma-run-id nor x-hekma-labels to the model
        /// provider (identity headers are on by default)
        #[arg(long)]
        no_identity_headers: bool,
        /// Host header values the metering proxy accepts besides 127.0.0.1,
        /// ::1 and localhost (comma-separated; any other Host is refused)
        #[arg(long, value_name = "NAME,...", value_delimiter = ',')]
        proxy_allowed_hosts: Vec<String>,
        /// Origin header values the metering proxy accepts (comma-separated
        /// origins such as https://app.example; by default a request with
        /// any Origin is refused, since browser pages send one)
        #[arg(long, value_name = "ORIGIN,...", value_delimiter = ',')]
        proxy_allowed_origins: Vec<String>,
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
        /// OPT-IN OTLP telemetry export: POST otlp-metrics v1 metric batches
        /// to this collector over OTLP/HTTP+JSON (http:// in v1 — https is a
        /// documented deferral). Default: off — no export task exists, no
        /// socket is dialed, nothing leaves the machine.
        #[arg(long, value_name = "URL")]
        otlp_endpoint: Option<String>,
        /// Headers for every OTLP POST, `name=value[,name=value...]`; a value
        /// may be a `secret:NAME` reference resolved at startup (unresolvable
        /// = a typed startup refusal; values never enter logs). Requires
        /// --otlp-endpoint.
        #[arg(long, value_name = "SPEC", requires = "otlp_endpoint")]
        otlp_headers: Option<String>,
        /// The export flush interval in whole seconds, 1..=3600 (default 10).
        /// Requires --otlp-endpoint (the exporter never half-exists — the
        /// same rule `--otlp-headers` carries, mirrored in
        /// `OtlpOptions::validate` for library callers).
        #[arg(
            long,
            value_name = "SECS",
            value_parser = clap::value_parser!(u64).range(1..=3_600),
            requires = "otlp_endpoint"
        )]
        otlp_flush_interval: Option<u64>,
        /// Serve --listen over HTTPS with this PEM certificate chain (with
        /// --tls-key). Required for a non-loopback --listen.
        #[arg(long, value_name = "PEM", requires_all = ["tls_key", "listen"])]
        tls_cert: Option<String>,
        /// The PEM private key for --tls-cert.
        #[arg(long, value_name = "PEM", requires_all = ["tls_cert", "listen"])]
        tls_key: Option<String>,
        /// Internal: the background host a `hekma agent` command starts when
        /// no host serves the state directory (loopback only; exits when
        /// idle).
        #[arg(
            long,
            hide = true,
            conflicts_with_all = ["listen", "bearer_secret", "tls_cert", "tls_key"]
        )]
        autostart: bool,
        /// Internal: the auto-started host's idle window in seconds (default
        /// 30).
        #[arg(
            long,
            hide = true,
            value_name = "SECS",
            requires = "autostart",
            value_parser = clap::value_parser!(u64).range(1..=86_400)
        )]
        idle_window: Option<u64>,
    },
}

#[derive(Subcommand)]
enum RunCommands {
    /// Open a child Run under a Run or an agent's open Run
    Open {
        /// The parent: a run id, or an agent name (its open Run)
        #[arg(long, value_name = "RUN_ID|AGENT")]
        parent: String,
        /// A caller label, key=value (repeatable)
        #[arg(long = "label", value_name = "KEY=VALUE")]
        labels: Vec<String>,
        /// A per-run token ceiling over the child and its own children
        #[arg(long, value_name = "TOKENS")]
        token_budget: Option<u64>,
        /// A per-run dollar cap over the child and its own children ("3.00")
        #[arg(long, value_name = "DOLLARS")]
        dollar_cap: Option<String>,
        /// The Breach Action for the child's ceilings (warn)
        #[arg(long, value_name = "ACTION")]
        breach_action: Option<String>,
        /// An id under which a retried open returns the same Run
        #[arg(long, value_name = "ID")]
        external_id: Option<String>,
        /// Issue the child's Run Key and refresh credential and print them
        /// (once — they are not stored and cannot be shown again)
        #[arg(long)]
        print_key: bool,
        /// Print the Run as JSON
        #[arg(long)]
        json: bool,
    },
    /// Close a child Run, or every child Run a Selector matches
    #[command(after_help = SET_VERB_AFTER_HELP)]
    Close {
        /// The run id
        #[arg(value_name = "RUN_ID", required_unless_present = "selector")]
        run_id: Option<String>,
        /// Act on every open Run carrying all these labels (key=value[,key=value...])
        #[arg(long, value_name = "SELECTOR", conflicts_with = "run_id")]
        selector: Option<String>,
        /// Show each Run's outcome and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Do not ask before acting on several Runs
        #[arg(long)]
        yes: bool,
        /// Print the Run (or the results) as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show one Run
    Show {
        /// The run id
        run_id: String,
        /// Print the Run as JSON
        #[arg(long)]
        json: bool,
    },
    /// List a Run's credentials: token ids, classes and states, never a key
    Keys {
        /// The run id
        run_id: String,
        /// Print the list as JSON
        #[arg(long)]
        json: bool,
    },
    /// Revoke a Run's credentials, and those issued under them
    RevokeKeys {
        /// The run id
        run_id: String,
        /// Revoke only the Run's own credentials, not those of the child
        /// Runs opened with them
        #[arg(long)]
        no_cascade: bool,
        /// Print what was revoked as JSON
        #[arg(long)]
        json: bool,
    },
}

/// The limit flags `hekma policy create` and `change` share.
#[derive(clap::Args, Clone, Debug, Default)]
struct PolicyLimitArgs {
    /// Per-run token ceiling
    #[arg(long, value_name = "N")]
    tokens_per_run: Option<u64>,
    /// Cumulative token ceiling (over the agent's life)
    #[arg(long, value_name = "N")]
    tokens_cumulative: Option<u64>,
    /// Per-run Cost Cap in dollars (an estimate from Rates you set)
    #[arg(long, value_name = "DOLLARS")]
    dollars_per_run: Option<String>,
    /// Cumulative Cost Cap in dollars (an estimate from Rates you set)
    #[arg(long, value_name = "DOLLARS")]
    dollars_cumulative: Option<String>,
    /// What a breach does: warn, pause, stop or block
    #[arg(long, value_name = "ACTION")]
    breach_action: Option<String>,
    /// The Scent-Check Mode for calls without a credential Hekma issued:
    /// off, shadow (serve and mark the Run) or enforce (refuse)
    #[arg(long, value_name = "MODE")]
    scent_check: Option<String>,
    /// Input price in dollars per 1M tokens
    #[arg(long, value_name = "DOLLARS")]
    rate_input: Option<String>,
    /// Output price in dollars per 1M tokens
    #[arg(long, value_name = "DOLLARS")]
    rate_output: Option<String>,
    /// Cached-input price in dollars per 1M tokens (defaults to the input price)
    #[arg(long, value_name = "DOLLARS")]
    rate_cached: Option<String>,
}

impl From<PolicyLimitArgs> for cli::policy::LimitFlags {
    fn from(args: PolicyLimitArgs) -> Self {
        Self {
            tokens_per_run: args.tokens_per_run,
            tokens_cumulative: args.tokens_cumulative,
            dollars_per_run: args.dollars_per_run,
            dollars_cumulative: args.dollars_cumulative,
            breach_action: args.breach_action,
            scent_check: args.scent_check,
            rate_input: args.rate_input,
            rate_output: args.rate_output,
            rate_cached: args.rate_cached,
        }
    }
}

// `list` carries every filter flag and `show` only an id; the enum lives
// once per invocation, so the size difference costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum CallsCommands {
    /// List per-call records, newest first
    List {
        /// Only this Run's calls (never its child Runs')
        #[arg(long, value_name = "RUN_ID")]
        run: Option<String>,
        /// Only calls that presented this Run Key (its 12-hex token id)
        #[arg(long, value_name = "TOKEN_ID")]
        token_id: Option<String>,
        /// Only calls with this outcome (completed, refused, upstream-error,
        /// client-disconnect, shutdown, crash, recovered)
        #[arg(long, value_name = "OUTCOME")]
        outcome: Option<String>,
        /// Only calls with this disposition (pass, block, hold, reroute,
        /// reject)
        #[arg(long, value_name = "DISPOSITION")]
        disposition: Option<String>,
        /// Only calls with this status class (2xx, 3xx, 4xx, 5xx, refused,
        /// transport-error)
        #[arg(long, value_name = "CLASS")]
        status_class: Option<String>,
        /// Only calls that named this model
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
        /// Only calls at this Control Depth (observed, wire, announced,
        /// supervised)
        #[arg(long, value_name = "DEPTH")]
        depth: Option<String>,
        /// Only calls made through this binding
        #[arg(long, value_name = "NAME")]
        binding: Option<String>,
        /// Only calls that started at or after this time (RFC 3339)
        #[arg(long, value_name = "TIME")]
        from: Option<String>,
        /// Only calls that started before this time (RFC 3339)
        #[arg(long, value_name = "TIME")]
        to: Option<String>,
        /// Only the calls that did not complete cleanly
        #[arg(long)]
        problems: bool,
        /// Page size (default 50, at most 1000; 0 means the maximum)
        #[arg(long, value_name = "N")]
        limit: Option<u64>,
        /// Continue after a previous page's cursor
        #[arg(long, value_name = "CURSOR")]
        cursor: Option<String>,
        /// Print the listing as one JSON document
        #[arg(long)]
        json: bool,
    },
    /// Show one per-call record
    Show {
        /// The call's id (call_ plus 26 characters)
        call_id: String,
        /// Print the record as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum BindingCommands {
    /// List every binding by name
    List {
        /// Continue after a previous page's last name
        #[arg(long, value_name = "NAME")]
        after: Option<String>,
        /// Page size (default 100, at most 256)
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Print the bindings as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show one binding, its client base URL included
    Show {
        /// The binding's name
        name: String,
        /// Print the binding as JSON
        #[arg(long)]
        json: bool,
    },
    /// Create a binding: calls on a path prefix forward to an upstream
    Create {
        /// The binding's name (lowercase letters, digits, - and _)
        name: String,
        /// The path prefix its calls arrive on (/ plus segments; / alone is
        /// the catch-all)
        #[arg(long, value_name = "PREFIX")]
        prefix: String,
        /// The upstream base URL they forward to (http:// or https://)
        #[arg(long, value_name = "URL")]
        upstream: String,
        /// A label its Runs carry (key=value; repeatable)
        #[arg(long, value_name = "K=V")]
        label: Vec<String>,
        /// A custodied key stored as a secret:NAME reference (inert until
        /// key custody ships)
        #[arg(long, value_name = "REF")]
        custodied_key: Option<String>,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Change a binding's prefix, upstream, labels or custodied key
    Set {
        /// The binding's name
        name: String,
        /// The new path prefix
        #[arg(long, value_name = "PREFIX")]
        prefix: Option<String>,
        /// The new upstream base URL
        #[arg(long, value_name = "URL")]
        upstream: Option<String>,
        /// A label its Runs carry (key=value; repeatable; the whole set)
        #[arg(long, value_name = "K=V")]
        label: Vec<String>,
        /// Remove every label
        #[arg(long)]
        clear_labels: bool,
        /// The new custodied-key reference
        #[arg(long, value_name = "REF", conflicts_with = "clear_custodied_key")]
        custodied_key: Option<String>,
        /// Remove the custodied-key reference
        #[arg(long)]
        clear_custodied_key: bool,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Delete a binding (its Run ends with cause binding-removed)
    Delete {
        /// The binding's name
        name: String,
    },
    /// Manage a binding's reroute routes
    Route {
        #[command(subcommand)]
        command: BindingRouteCommands,
    },
    /// Manage a binding's announce and proxy-access credentials
    Credential {
        #[command(subcommand)]
        command: BindingCredentialCommands,
    },
}

#[derive(Subcommand)]
enum BindingRouteCommands {
    /// Add or replace one reroute route (used when reroute ships)
    Add {
        /// The binding's name
        name: String,
        /// The route's name
        route: String,
        /// The upstream base URL the route forwards to
        #[arg(long, value_name = "URL")]
        upstream: String,
        /// A custodied key stored as a secret:NAME reference
        #[arg(long, value_name = "REF")]
        custodied_key: Option<String>,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove one reroute route
    Remove {
        /// The binding's name
        name: String,
        /// The route's name
        route: String,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum BindingCredentialCommands {
    /// Issue a binding credential (shown once; rotates the previous one)
    Issue {
        /// The binding's name
        name: String,
        /// The credential class: announce or proxy-access
        #[arg(long, value_name = "CLASS")]
        class: String,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Revoke a binding's active credential of a class
    Revoke {
        /// The binding's name
        name: String,
        /// The credential class: announce or proxy-access
        #[arg(long, value_name = "CLASS")]
        class: String,
    },
    /// List a binding's credentials (token ids and states only)
    List {
        /// The binding's name
        name: String,
        /// Print the credentials as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum PolicyCommands {
    /// List every Policy, the default Policy included
    List {
        /// Print the Policies as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show one Policy
    Show {
        /// The Policy's name
        name: String,
        /// Print the Policy as JSON
        #[arg(long)]
        json: bool,
    },
    /// Create a Policy for the Runs a Selector matches by bound label
    Create {
        /// The Policy's name (lowercase letters, digits, - and _)
        name: String,
        /// The bound labels of the Runs it governs (key=value[,key=value...])
        #[arg(long, value_name = "SELECTOR")]
        selector: String,
        #[command(flatten)]
        limits: PolicyLimitArgs,
        /// Show what it would match and write nothing
        #[arg(long)]
        dry_run: bool,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Change a Policy, the default Policy included
    Change {
        /// The Policy's name (`default` for the default Policy)
        name: String,
        /// A new Selector (not on the default Policy)
        #[arg(long, value_name = "SELECTOR")]
        selector: Option<String>,
        #[command(flatten)]
        limits: PolicyLimitArgs,
        /// Limits to clear: tokens-per-run, tokens-cumulative, dollars-per-run,
        /// dollars-cumulative, breach-action, rate, or rate:<model>
        #[arg(long, value_name = "LIMIT", value_delimiter = ',')]
        clear: Vec<String>,
        /// On the default Policy: the model the --rate-* flags price
        #[arg(long, value_name = "MODEL")]
        rate_for: Option<String>,
        /// Show what it would change and write nothing
        #[arg(long)]
        dry_run: bool,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
    /// Delete a Policy (the default Policy cannot be deleted)
    Delete {
        /// The Policy's name
        name: String,
        /// Show what it would re-resolve and write nothing
        #[arg(long)]
        dry_run: bool,
        /// Print the result as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum LabelCommands {
    /// Bind labels on a Run, an agent's open Run, or every Run a Selector matches
    Set {
        /// The target (a run id or an agent, unless --selector is given),
        /// then the labels as key=value
        #[arg(value_name = "TARGET|KEY=VALUE", required = true)]
        args: Vec<String>,
        /// Act on every open Run carrying all these labels (key=value[,key=value...])
        #[arg(long, value_name = "SELECTOR")]
        selector: Option<String>,
        /// Show each Run's change and write nothing
        #[arg(long)]
        dry_run: bool,
        /// Do not ask before acting on several Runs
        #[arg(long)]
        yes: bool,
        /// Print the results as JSON
        #[arg(long)]
        json: bool,
    },
    /// Unbind labels set with `hekma label set`
    Remove {
        /// The target (a run id or an agent, unless --selector is given),
        /// then the label keys
        #[arg(value_name = "TARGET|KEY", required = true)]
        args: Vec<String>,
        /// Act on every open Run carrying all these labels (key=value[,key=value...])
        #[arg(long, value_name = "SELECTOR")]
        selector: Option<String>,
        /// Show each Run's change and write nothing
        #[arg(long)]
        dry_run: bool,
        /// Do not ask before acting on several Runs
        #[arg(long)]
        yes: bool,
        /// Print the results as JSON
        #[arg(long)]
        json: bool,
    },
}

/// The Selector flags the set-taking lifecycle verbs share.
#[derive(clap::Args, Clone, Debug, Default)]
struct SetFlags {
    /// Act on every open Run carrying all these labels (key=value[,key=value...])
    #[arg(long, value_name = "SELECTOR", conflicts_with = "name")]
    selector: Option<String>,
    /// Show each Run's outcome and change nothing
    #[arg(long)]
    dry_run: bool,
    /// Do not ask before acting on several Runs
    #[arg(long)]
    yes: bool,
    /// Print the results as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum BudgetCommands {
    /// Show a Run's limits: each ceiling and where it comes from, usage, headroom and latches
    Show {
        /// A run id or an agent (its open Run); without one, every Run is listed
        #[arg(value_name = "TARGET")]
        target: Option<String>,
        /// Show every Run carrying all these labels (key=value[,key=value...])
        #[arg(long, value_name = "SELECTOR", conflicts_with = "target")]
        selector: Option<String>,
        /// How many Runs to show per page when listing (default 50, at most 1000)
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Continue a listing where a previous page stopped (the cursor it printed)
        #[arg(long, value_name = "CURSOR")]
        cursor: Option<String>,
        /// Print the limits as JSON
        #[arg(long)]
        json: bool,
    },
    /// Set or clear limits on a Run, an agent's open Run, or every Run a Selector matches
    Set {
        /// A run id or an agent (its open Run), unless --selector is given
        #[arg(value_name = "TARGET", required_unless_present = "selector")]
        target: Option<String>,
        /// Act on every open Run carrying all these labels (key=value[,key=value...])
        #[arg(long, value_name = "SELECTOR", conflicts_with = "target")]
        selector: Option<String>,
        /// Per-run token ceiling
        #[arg(long, value_name = "N")]
        tokens_per_run: Option<u64>,
        /// Cumulative token ceiling (the agent's own Run only)
        #[arg(long, value_name = "N")]
        tokens_cumulative: Option<u64>,
        /// Per-run dollar cap ("3.00")
        #[arg(long, value_name = "DOLLARS")]
        dollars_per_run: Option<String>,
        /// Cumulative dollar cap ("3.00"; the agent's own Run only)
        #[arg(long, value_name = "DOLLARS")]
        dollars_cumulative: Option<String>,
        /// The Breach Action: warn, pause, stop or block
        #[arg(long, value_name = "ACTION")]
        breach_action: Option<String>,
        /// Clear limits set on the Run (tokens-per-run, tokens-cumulative,
        /// dollars-per-run, dollars-cumulative, breach-action; comma-separated
        /// or repeated)
        #[arg(long, value_name = "LIMIT")]
        clear: Vec<String>,
        /// Show each Run's change and write nothing
        #[arg(long)]
        dry_run: bool,
        /// Do not ask before acting on several Runs
        #[arg(long)]
        yes: bool,
        /// Print the results as JSON
        #[arg(long)]
        json: bool,
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
        /// Detach the agent from the local Service Host's lifetime: it keeps
        /// running even if the host exits (every start already outlives this
        /// command). Supervised while the host runs; after a host exit the
        /// next host re-adopts it, and until then there is no crash
        /// detection, no budget enforcement and no event delivery. Refused
        /// for engine-observed instances (the metering proxy they call lives
        /// in the host).
        #[arg(long)]
        detach: bool,
    },
    /// Stop a running Agent Instance (graceful, then forced after the window)
    #[command(after_help = SET_VERB_AFTER_HELP)]
    Stop {
        /// The Agent Instance to stop, or a run id (its agent's Run)
        #[arg(value_name = "TARGET", required_unless_present = "selector")]
        name: Option<String>,
        #[command(flatten)]
        set: SetFlags,
        /// Graceful-shutdown window in seconds before a forced kill (default
        /// 30; served within 1 to 86400)
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Pause a running Agent Instance (honest per-OS: guaranteed/best-effort/unsupported)
    #[command(after_help = SET_VERB_AFTER_HELP)]
    Pause {
        /// The Agent Instance to pause, or a run id (its agent's Run)
        #[arg(value_name = "TARGET", required_unless_present = "selector")]
        name: Option<String>,
        #[command(flatten)]
        set: SetFlags,
    },
    /// Resume a paused Agent Instance
    #[command(after_help = SET_VERB_AFTER_HELP)]
    Resume {
        /// The Agent Instance to resume, or a run id (its agent's Run)
        #[arg(value_name = "TARGET", required_unless_present = "selector")]
        name: Option<String>,
        #[command(flatten)]
        set: SetFlags,
    },
    /// Drain a running Agent Instance: pause it via its adapter's declared
    /// `[lifecycle.pause]` COMMAND (cooperative — no OS signal), wait —
    /// bounded — for the declared idle signal, then stop it. A drain that
    /// outlives its bound STILL stops (the outcome is reported honestly).
    #[command(after_help = SET_VERB_AFTER_HELP)]
    Drain {
        /// The Agent Instance to drain, or a run id (its agent's Run)
        #[arg(value_name = "TARGET", required_unless_present = "selector")]
        name: Option<String>,
        #[command(flatten)]
        set: SetFlags,
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
            no_identity_headers,
            proxy_allowed_hosts,
            proxy_allowed_origins,
            state_dir,
            listen,
            bearer_secret,
            log_capture,
            otlp_endpoint,
            otlp_headers,
            otlp_flush_interval,
            tls_cert,
            tls_key,
            autostart,
            idle_window,
        }) => cli::serve::run(cli::serve::ServeFlags {
            stop_window,
            metering_listen,
            no_identity_headers,
            proxy_allowed_hosts,
            proxy_allowed_origins,
            state_dir,
            listen,
            bearer_secret,
            // Review finding 19: the flag passes through as an Option —
            // `serve::run`'s single `unwrap_or_default` is THE defaulting
            // site (no clap `default_value` duplicates it).
            log_capture,
            otlp_endpoint,
            otlp_headers,
            otlp_flush_interval,
            tls_cert,
            tls_key,
            autostart,
            idle_window,
        }),
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
            AgentCommands::Stop { name, set, timeout } => cli::lifecycle::run(
                hekma_engine::RunLifecycleVerb::Stop,
                lifecycle_args(name, set, timeout, None),
            ),
            AgentCommands::Pause { name, set } => cli::lifecycle::run(
                hekma_engine::RunLifecycleVerb::Pause,
                lifecycle_args(name, set, None, None),
            ),
            AgentCommands::Resume { name, set } => cli::lifecycle::run(
                hekma_engine::RunLifecycleVerb::Resume,
                lifecycle_args(name, set, None, None),
            ),
            AgentCommands::Drain {
                name,
                set,
                timeout_ms,
            } => cli::lifecycle::run(
                hekma_engine::RunLifecycleVerb::Drain,
                lifecycle_args(name, set, None, timeout_ms),
            ),
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
        Some(Commands::Run { command }) => match command {
            RunCommands::Open {
                parent,
                labels,
                token_budget,
                dollar_cap,
                breach_action,
                external_id,
                print_key,
                json,
            } => cli::run::open(cli::run::OpenArgs {
                parent,
                labels,
                token_budget,
                dollar_cap,
                breach_action,
                external_id,
                print_key,
                json,
            }),
            RunCommands::Close {
                run_id,
                selector,
                dry_run,
                yes,
                json,
            } => cli::lifecycle::close(cli::lifecycle::LifecycleArgs {
                target: run_id,
                selector,
                dry_run,
                yes,
                json,
                timeout_secs: None,
                timeout_ms: None,
            }),
            RunCommands::Show { run_id, json } => cli::run::show(&run_id, json),
            RunCommands::Keys { run_id, json } => cli::run::keys(&run_id, json),
            RunCommands::RevokeKeys {
                run_id,
                no_cascade,
                json,
            } => cli::run::revoke_keys(&run_id, !no_cascade, json),
        },
        Some(Commands::Calls { command }) => match command {
            CallsCommands::List {
                run,
                token_id,
                outcome,
                disposition,
                status_class,
                model,
                depth,
                binding,
                from,
                to,
                problems,
                limit,
                cursor,
                json,
            } => cli::calls::list(cli::calls::ListArgs {
                run,
                token_id,
                outcome,
                disposition,
                status_class,
                model,
                depth,
                binding,
                from,
                to,
                problems,
                limit,
                cursor,
                json,
            }),
            CallsCommands::Show { call_id, json } => cli::calls::show(&call_id, json),
        },
        Some(Commands::Binding { command }) => match command {
            BindingCommands::List { after, limit, json } => {
                cli::binding::list(after, limit.unwrap_or(100), json)
            }
            BindingCommands::Show { name, json } => cli::binding::show(&name, json),
            BindingCommands::Create {
                name,
                prefix,
                upstream,
                label,
                custodied_key,
                json,
            } => cli::binding::create(cli::binding::CreateArgs {
                name,
                prefix,
                upstream,
                labels: label,
                custodied_key,
                json,
            }),
            BindingCommands::Set {
                name,
                prefix,
                upstream,
                label,
                clear_labels,
                custodied_key,
                clear_custodied_key,
                json,
            } => cli::binding::set(cli::binding::SetArgs {
                name,
                prefix,
                upstream,
                labels: label,
                clear_labels,
                custodied_key,
                clear_custodied_key,
                json,
            }),
            BindingCommands::Delete { name } => cli::binding::delete(&name),
            BindingCommands::Route { command } => match command {
                BindingRouteCommands::Add {
                    name,
                    route,
                    upstream,
                    custodied_key,
                    json,
                } => cli::binding::route_add(cli::binding::RouteArgs {
                    name,
                    route,
                    upstream,
                    custodied_key,
                    json,
                }),
                BindingRouteCommands::Remove { name, route, json } => {
                    cli::binding::route_remove(&name, &route, json)
                }
            },
            BindingCommands::Credential { command } => match command {
                BindingCredentialCommands::Issue { name, class, json } => {
                    cli::binding::credential_issue(&name, &class, json)
                }
                BindingCredentialCommands::Revoke { name, class } => {
                    cli::binding::credential_revoke(&name, &class)
                }
                BindingCredentialCommands::List { name, json } => {
                    cli::binding::credential_list(&name, json)
                }
            },
        },
        Some(Commands::Policy { command }) => match command {
            PolicyCommands::List { json } => cli::policy::list(json),
            PolicyCommands::Show { name, json } => cli::policy::show(&name, json),
            PolicyCommands::Create {
                name,
                selector,
                limits,
                dry_run,
                json,
            } => cli::policy::create(cli::policy::WriteArgs {
                name,
                selector: Some(selector),
                limits: limits.into(),
                clear: Vec::new(),
                rate_for: None,
                dry_run,
                json,
            }),
            PolicyCommands::Change {
                name,
                selector,
                limits,
                clear,
                rate_for,
                dry_run,
                json,
            } => cli::policy::change(cli::policy::WriteArgs {
                name,
                selector,
                limits: limits.into(),
                clear,
                rate_for,
                dry_run,
                json,
            }),
            PolicyCommands::Delete {
                name,
                dry_run,
                json,
            } => cli::policy::delete(&name, dry_run, json),
        },
        Some(Commands::Hold {
            target,
            selector,

            dry_run,
            yes,
            json,
        }) => cli::hold::run(
            cli::hold::HoldVerb::Hold,
            cli::hold::HoldArgs {
                target,
                selector,

                dry_run,
                yes,
                json,
                ..Default::default()
            },
        ),
        Some(Commands::Release {
            target,
            selector,

            dry_run,
            yes,
            json,
        }) => cli::hold::run(
            cli::hold::HoldVerb::Release,
            cli::hold::HoldArgs {
                target,
                selector,

                dry_run,
                yes,
                json,
                ..Default::default()
            },
        ),
        Some(Commands::Reroute {
            target,
            selector,
            via,
            off,
            dry_run,
            yes,
            json,
        }) => cli::hold::run(
            cli::hold::HoldVerb::Reroute,
            cli::hold::HoldArgs {
                target,
                selector,
                via,
                off,
                dry_run,
                yes,
                json,
            },
        ),
        Some(Commands::Reject {
            target,
            selector,

            dry_run,
            yes,
            json,
        }) => cli::hold::run(
            cli::hold::HoldVerb::Reject,
            cli::hold::HoldArgs {
                target,
                selector,

                dry_run,
                yes,
                json,
                ..Default::default()
            },
        ),
        Some(Commands::Budget { command }) => match command {
            BudgetCommands::Show {
                target,
                selector,
                limit,
                cursor,
                json,
            } => cli::budget::show(cli::budget::ShowArgs {
                target,
                selector,
                limit,
                cursor,
                json,
            }),
            BudgetCommands::Set {
                target,
                selector,
                tokens_per_run,
                tokens_cumulative,
                dollars_per_run,
                dollars_cumulative,
                breach_action,
                clear,
                dry_run,
                yes,
                json,
            } => cli::budget::set(cli::budget::SetArgs {
                target,
                selector,
                tokens_per_run,
                tokens_cumulative,
                dollars_per_run,
                dollars_cumulative,
                breach_action,
                clear,
                dry_run,
                yes,
                json,
            }),
        },
        Some(Commands::Ps {
            selector,
            all,
            limit,
            cursor,
            json,
        }) => cli::ps::run(cli::ps::PsArgs {
            selector,
            all,
            limit,
            cursor,
            json,
        }),
        Some(Commands::Label { command }) => {
            let (verb, args, selector, dry_run, yes, json) = match command {
                LabelCommands::Set {
                    args,
                    selector,
                    dry_run,
                    yes,
                    json,
                } => (
                    cli::label::LabelVerb::Set,
                    args,
                    selector,
                    dry_run,
                    yes,
                    json,
                ),
                LabelCommands::Remove {
                    args,
                    selector,
                    dry_run,
                    yes,
                    json,
                } => (
                    cli::label::LabelVerb::Remove,
                    args,
                    selector,
                    dry_run,
                    yes,
                    json,
                ),
            };
            cli::label::run(cli::label::LabelArgs {
                verb,
                selector,
                args,
                dry_run,
                yes,
                json,
            })
        }
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

/// The lifecycle verbs' shared arguments from their flags.
fn lifecycle_args(
    name: Option<String>,
    set: SetFlags,
    timeout_secs: Option<u64>,
    timeout_ms: Option<u64>,
) -> cli::lifecycle::LifecycleArgs {
    cli::lifecycle::LifecycleArgs {
        target: name,
        selector: set.selector,
        dry_run: set.dry_run,
        yes: set.yes,
        json: set.json,
        timeout_secs,
        timeout_ms,
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

    #[test]
    fn test_serve_otlp_flags_parse_and_headers_require_the_endpoint() {
        // The OTLP flags are pinned at the clap surface too: the endpoint
        // lands in its field, the headers ride as the RAW spec string (the
        // parse into typed values happens once, in serve::run), the
        // interval has clap-range bounds, and `--otlp-headers` without
        // `--otlp-endpoint` is refused (the exporter never half-exists).
        let cli = Cli::try_parse_from([
            "hekma",
            "serve",
            "--otlp-endpoint",
            "http://127.0.0.1:4318",
            "--otlp-headers",
            "authorization=secret:OTLP_AUTH",
            "--otlp-flush-interval",
            "30",
        ])
        .expect("the otlp flag set parses");
        let Some(Commands::Serve {
            otlp_endpoint,
            otlp_headers,
            otlp_flush_interval,
            ..
        }) = &cli.command
        else {
            panic!("expected the serve subcommand");
        };
        assert_eq!(
            otlp_endpoint.as_deref(),
            Some("http://127.0.0.1:4318"),
            "the endpoint lands verbatim"
        );
        assert_eq!(
            otlp_headers.as_deref(),
            Some("authorization=secret:OTLP_AUTH"),
            "the headers spec rides RAW (parsed once in serve::run)"
        );
        assert_eq!(*otlp_flush_interval, Some(30));

        let bare = Cli::try_parse_from(["hekma", "serve"]).expect("bare serve parses");
        let Some(Commands::Serve { otlp_endpoint, .. }) = &bare.command else {
            panic!("expected the serve subcommand");
        };
        assert_eq!(
            *otlp_endpoint, None,
            "no --otlp-endpoint parses as None — the exporter is ABSENT by default"
        );

        match Cli::try_parse_from(["hekma", "serve", "--otlp-headers", "a=b"]) {
            Ok(_) => panic!("--otlp-headers without --otlp-endpoint must be refused"),
            Err(err) => assert!(
                err.use_stderr(),
                "half-configured export is a usage error (the exporter never half-exists)"
            ),
        }
        match Cli::try_parse_from([
            "hekma",
            "serve",
            "--otlp-endpoint",
            "http://h",
            "--otlp-flush-interval",
            "0",
        ]) {
            Ok(_) => panic!("an out-of-bounds flush interval must be a parse refusal"),
            Err(err) => assert!(err.use_stderr(), "the clap range refuses 0"),
        }
    }
}
