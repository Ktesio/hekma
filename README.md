<p align="center">
  <img src="https://raw.githubusercontent.com/Ktesio/hekma/main/docs/assets/hekma-banner.png" alt="Hekma banner: the control plane for agent workloads" width="100%">
</p>

# Hekma

[![CI](https://github.com/Ktesio/hekma/actions/workflows/ci.yml/badge.svg)](https://github.com/Ktesio/hekma/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/hekma.svg)](https://crates.io/crates/hekma)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](https://github.com/Ktesio/hekma/blob/main/LICENSE)

**Hekma is the control plane for agent workloads.** One way to run, see and steer any agent — a coding agent such as OpenCode, a personal agent such as Hermes Agent, a CrewAI or LangGraph flow, or a raw model call — from a laptop to a fleet, built so that it is never the reason prod stops. It is a single Rust binary for macOS, Linux and Windows, and an engine you can embed in your own Rust program.

Hekma is built in the open, in stages. **Today, in 0.11.0,** it supervises, meters and budgets the agents you run. **Runs, Policies, Run Keys and wire control are being built now**; they are marked *in development* or *planned* below and in the docs, and their names and defaults may change before they ship.

**Docs: [hekma.ktesio.dev/docs](https://hekma.ktesio.dev/docs/)**, starting with the [five-minute quickstart](https://hekma.ktesio.dev/docs/get-started/). The [roadmap](https://hekma.ktesio.dev/docs/status/#roadmap) shows what lands next.

An *agent* here is a program that calls a language model on your behalf. Hekma works on it from the outside — as a process it runs, or, soon, as traffic and telemetry it sees — and does not help you write agents.

## How Hekma is built to behave

These are the rules the control plane is being built to; most of what makes them true is *in development* or *planned* (see below).

- **Signal, don't stop.** A crossed limit will be made loud — in listings, events, metrics and response headers — so your framework or your on-call can act. Pausing, stopping or fencing a run is something you choose, never a surprise. *(The `warn` default is in development; in 0.11.0 the default breach action is `pause`.)*
- **One interface, whatever the agent.** The same verbs, API and telemetry for every agent technology, so changing frameworks does not change your tooling.
- **Operators set the limits.** An agent or framework will be able to tighten what you configured, never loosen it.
- **OpenTelemetry first.** Hekma feeds the observability stack you already run (OTLP export is on main, unreleased); its own surfaces are the CLI, for emergencies, and the Control API, for daily work.
- **Built for scale.** The goal is one agent on a laptop to millions of Runs, with exception-first views that show you only what needs you.

## What ships today

| | |
|---|---|
| **Lifecycle** | `start`, `stop`, `pause` and `resume` any agent through one state machine. Crashes are recorded and restarted with backoff, and the fleet's state survives a reboot. |
| **Metering** | Counts input, output and cached tokens, either reported by the agent or read off the wire by a proxy on `127.0.0.1`. |
| **Telemetry export** *(unreleased: on main, after 0.11.0)* | Opt-in OTLP/HTTP+JSON metrics export to any OpenTelemetry-compatible collector (`--otlp-endpoint` on `hekma serve`) — token usage, budget breaches, derived cost, lifecycle state. Off by default: no endpoint, no exporter. [Telemetry docs](https://hekma.ktesio.dev/docs/telemetry/). |
| **Budgets** | Token and dollar ceilings, per run or over the agent's life, with the action you choose — `warn`, `pause` or `stop` — applied at the usage event that crossed the line. Dollar figures are estimates from rates you set. |
| **Any agent** | A built-in `acp` kind for Agent Client Protocol agents (OpenCode and Hermes Agent are tested against it), and an `adapter.toml` manifest for anything else you can start as a process. The [adapter gallery](https://hekma.ktesio.dev/docs/adapters/) is the validated index of which agents run today, with per-capability honesty tables. |
| **Scripting** | Versioned JSON output and stable exit codes on every command. |
| **Embedding** | The engine is the `hekma-engine` crate, with an event stream, so your program can run the fleet without the CLI. |

## Being built

None of this is in 0.11.0 yet. Each item links to its docs page, which is tagged with its stage.

| | Stage | |
|---|---|---|
| **Runs** | in development | Every supervised agent, and the work a Host opens inside one, becomes a Run with an id, labels, a status and a Control Depth; Runs Hekma did not start follow (planned). `hekma ps` lists them exception-first, and Selectors let every verb act on `flow=claims,env=prod` instead of one agent at a time. [Runs](https://hekma.ktesio.dev/docs/runs/) |
| **Policies** | in development | Standing limits matched by label, with `warn` as the default breach action and an opt-in `block` fence that refuses further model calls. [Policies](https://hekma.ktesio.dev/docs/policies/) |
| **Always-on host** | in development | `hekma serve` (on main) supervises all the time, and the CLI starts a local one on demand, so agents outlive your terminal. [Run Hekma as a service](https://hekma.ktesio.dev/docs/hosting/) |
| **Run Keys** | planned | A per-Run key in the agent's API-key slot; the real provider key never leaves Hekma, and a call around the proxy is flagged. [Run Keys](https://hekma.ktesio.dev/docs/run-keys/) |
| **Attach and wire control** | planned | Framework flows join through one shared proxy, a header or inbound OpenTelemetry; hold a Run's next model call with its state intact, then release, reroute or reject it. [Attach](https://hekma.ktesio.dev/docs/attach/) |
| **Never the reason prod stops** | planned | Agents survive host restarts, upgrades and crashes through a small shim per process, and `hekma top` shows it all live. [Roadmap](https://hekma.ktesio.dev/docs/status/#roadmap) |

## Install

macOS and Linux:

```bash
curl -fsSL https://cli.ktesio.dev/hekma/install.sh | sh
```

Windows (PowerShell):

```powershell
irm https://cli.ktesio.dev/hekma/install.ps1 | iex
```

The installer uses Homebrew or Cargo when you have them, and otherwise downloads a checksum-verified release binary. You get the `hekma` command and its short alias, `hkm`. Other ways to install:

```bash
brew install ktesio/tap/hekma     # Homebrew
cargo install hekma               # crates.io (Rust 1.96.1 or later)
```

To build from source:

```bash
git clone https://github.com/Ktesio/hekma.git
cd hekma
cargo install --path crates/hekma
```

Release archives for each platform are on [GitHub Releases](https://github.com/Ktesio/hekma/releases). The [install guide](https://hekma.ktesio.dev/docs/installation/) covers updates, uninstalling and where Hekma keeps its state. Upgrading from the old `kt` command? See [Upgrade from kt](https://hekma.ktesio.dev/docs/migration/); your data stays where it is.

## A first look

```bash
# Describe the agent once, in a folder holding its adapter.toml.
hekma agent register my-agent --manifest ~/agents/my-agent

# Optional: a ceiling that only records a warning when crossed (pause is the 0.11.0 default).
hekma agent config set my-agent budget.tokens.cumulative 500000
hekma agent config set my-agent budget.breach_action warn

# Run it in the background, and watch it. Metering and budgets run live.
hekma agent start my-agent --detach
hekma agent logs my-agent --follow

# See the fleet: state, supervision and token usage for every agent.
hekma agent list
```

The [quickstart](https://hekma.ktesio.dev/docs/get-started/) walks through this with a small stand-in agent you can run as is, and [Bring your own agent](https://hekma.ktesio.dev/docs/your-own-agent/) shows how to write the `adapter.toml` for a real one.

**Know this before you rely on it.** In 0.11.0, the CLI supervises an agent only while some `hekma` command is running. A detached agent keeps running between commands, but nothing meters it or enforces its budget until the next command, which is why the example keeps `logs --follow` open. A program that embeds the engine supervises for as long as it runs, and a long-running host, `hekma serve`, is on the main branch for the next release. [Supervision and detached runs](https://hekma.ktesio.dev/docs/detached-runs/) explains the patterns that work today.

Letting an agent loose in CI? The [Agents in GitHub Actions](https://hekma.ktesio.dev/docs/agents-in-ci/) recipe caps its spend with a token budget, bounds the job on the wall clock, and lands its token and dollar usage in the step summary.

## Commands

| Command | What it does |
|---|---|
| `hekma agent register` | Add an agent from a built-in kind (`--kind acp`) or an `adapter.toml` (`--manifest`) |
| `hekma agent start`, `stop`, `pause`, `resume` | Drive the lifecycle (`start --detach` keeps the agent running after the command exits) |
| `hekma agent list`, `show`, `usage` | See state, budgets and token usage (`--json` for scripts) |
| `hekma agent logs` | Read an agent's output, or follow it live (`--follow`) |
| `hekma agent config set`, `get` | Set and read configuration, including budgets, rates and `secret:NAME` references |
| `hekma agent send` | Send a line of input to a running agent |
| `hekma agent memory attach`, `detach` | Give an agent durable memory |
| `hekma agent remove` | Remove an agent |
| `hekma self-update` | Update Hekma through the channel you installed it with |

Coming with Runs and Policies *(in development)*: `hekma ps`, `hekma label` and `hekma policy`, with `--selector` on every verb.

The [CLI reference](https://hekma.ktesio.dev/docs/commands/) has every flag, the [exit codes](https://hekma.ktesio.dev/docs/exit-codes/) and the [JSON documents](https://hekma.ktesio.dev/docs/json-output/).

## Embed the engine

```toml
[dependencies]
hekma-engine = "0.7"
```

The engine runs in your process with a blocking or async API, an event stream, and no global state. Your program then holds the engine for as long as it runs, so supervision, budgets, `send` and ACP prompt turns work continuously. See [Embed the engine](https://hekma.ktesio.dev/docs/embedding/).

## Write an adapter

An adapter tells Hekma how to launch an agent, what it supports on each OS, and where its token counts come from. Most agents need only an `adapter.toml` ([reference](https://hekma.ktesio.dev/docs/manifest/)). Before writing one, check the [adapter gallery](https://hekma.ktesio.dev/docs/adapters/) — ten seed agents (Copilot CLI, Hermes Agent, opencode, Claude Code, Codex, Gemini CLI, goose, Crush, Aider, Cursor CLI) already carry an honesty table and, for Aider, a CI-validated manifest you can copy. The conformance kit, `hekma-conformance`, checks an adapter against the versioned [adapter contract](https://hekma.ktesio.dev/docs/adapter-contract/) from an ordinary `cargo test`. It is not on crates.io yet, so depend on it by git, pinned to a release commit:

```toml
[dev-dependencies]
hekma-conformance = { git = "https://github.com/Ktesio/hekma", rev = "14fbf772710e9151f7a90be8d504a4f67bd95531" }
```

```rust
#[test]
fn my_adapter_conforms() {
    let report = hekma_conformance::run_mock_conformance(std::path::Path::new("adapter-dir"));
    assert!(report.is_conformant(), "failures = {:?}", report.failures());
}
```

## Status

Hekma is at 0.11.0 and moving quickly toward the control plane described above. The CLI, the JSON output, the exit codes and the adapter contract are compatibility surfaces: a breaking change is announced at least one minor release ahead and lands only in a major release. The coming change of the default breach action to `warn` is announced in the release notes before it ships, and agents that rely on today's `pause` default keep it. [Status and roadmap](https://hekma.ktesio.dev/docs/status/) lists what ships today, what is on the main branch and what is being built, and the [release notes](https://hekma.ktesio.dev/docs/release-notes/) cover each release.

## Contributing

Bug reports, ideas and pull requests are welcome. Read [CONTRIBUTING.md](https://github.com/Ktesio/hekma/blob/main/CONTRIBUTING.md) first. Contributions are made under the [Contributor License Agreement](https://github.com/Ktesio/hekma/blob/main/CLA.md), which keeps the project under one owner. Maintainers' guides to [testing](https://github.com/Ktesio/hekma/blob/main/docs/testing.md) and [releasing](https://github.com/Ktesio/hekma/blob/main/docs/release-process.md) live in this repository.

## License

Hekma is open source under the [Apache License 2.0](https://github.com/Ktesio/hekma/blob/main/LICENSE): free to use, change and distribute, including commercially, with an explicit patent grant. The license does not cover the Hekma or Ktesio names; see [TRADEMARK.md](https://github.com/Ktesio/hekma/blob/main/TRADEMARK.md). Releases up to 0.8.1 shipped under an earlier source-available license and keep it.

Hekma is a [Ktesio](https://ktesio.com) project, developed in the open at [ktesio.dev](https://ktesio.dev).
