<p align="center">
  <img src="https://raw.githubusercontent.com/Ktesio/hekma/main/docs/assets/hekma-banner.png" alt="Hekma banner: run AI agents like services" width="100%">
</p>

# Hekma

[![CI](https://github.com/Ktesio/hekma/actions/workflows/ci.yml/badge.svg)](https://github.com/Ktesio/hekma/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/hekma.svg)](https://crates.io/crates/hekma)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](https://github.com/Ktesio/hekma/blob/main/LICENSE)

Hekma runs the AI agents you already use like services. It starts, stops, pauses and restarts them, counts the tokens they spend, and pauses or stops an agent as soon as it crosses a token or dollar budget. It is a single Rust binary for macOS, Linux and Windows, and an engine you can embed in your own Rust program.

**Docs: [hekma.ktesio.dev/docs](https://hekma.ktesio.dev/docs/)**, starting with the [five-minute quickstart](https://hekma.ktesio.dev/docs/get-started/).

An *agent* here is a program that calls a language model on your behalf: a coding agent such as OpenCode, a personal agent such as Hermes Agent, or a script of your own. Hekma runs it as a process and governs it from the outside. It does not help you write agents.

## What it does

| | |
|---|---|
| **Lifecycle** | `start`, `stop`, `pause` and `resume` any agent through one state machine. Crashes are recorded and restarted with backoff, and the fleet's state survives a reboot. |
| **Metering** | Counts input, output and cached tokens, either reported by the agent or read off the wire by a proxy on `127.0.0.1`. |
| **Budgets** | Token and dollar ceilings, per run or over the agent's life, with a `pause`, `stop` or `warn` action applied at the usage event that crossed the line. Dollar figures are estimates from rates you set. |
| **Any agent** | A built-in `acp` kind for Agent Client Protocol agents (OpenCode and Hermes Agent are tested against it), and an `adapter.toml` manifest for anything else you can start as a process. The [adapter gallery](https://hekma.ktesio.dev/docs/adapters/) is the validated index of which agents run today, with per-capability honesty tables. |
| **Scripting** | Versioned JSON output and stable exit codes on every command. |
| **Embedding** | The engine is the `hekma-engine` crate, with an event stream, so your program can run the fleet without the CLI. |

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
hekma agent register my-agent --manifest `/agents/my-agent

# Give it a budget: pause it after 500,000 tokens.
hekma agent config set my-agent budget.tokens.cumulative 500000
hekma agent config set my-agent budget.breach_action pause

# Run it in the background, and watch it. Metering and budgets run live.
hekma agent start my-agent --detach
hekma agent logs my-agent --follow

# See the fleet: state, budget and token usage for every agent.
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

Hekma is at 0.11.0 and moving quickly. The CLI, the JSON output, the exit codes and the adapter contract are compatibility surfaces: a breaking change is announced at least one minor release ahead and lands only in a major release. [Status and roadmap](https://hekma.ktesio.dev/docs/status/) lists what ships today and what is on the main branch, and the [release notes](https://hekma.ktesio.dev/docs/release-notes/) cover each release.

## Contributing

Bug reports, ideas and pull requests are welcome. Read [CONTRIBUTING.md](https://github.com/Ktesio/hekma/blob/main/CONTRIBUTING.md) first. Contributions are made under the [Contributor License Agreement](https://github.com/Ktesio/hekma/blob/main/CLA.md), which keeps the project under one owner. Maintainers' guides to [testing](https://github.com/Ktesio/hekma/blob/main/docs/testing.md) and [releasing](https://github.com/Ktesio/hekma/blob/main/docs/release-process.md) live in this repository.

## License

Hekma is open source under the [Apache License 2.0](https://github.com/Ktesio/hekma/blob/main/LICENSE): free to use, change and distribute, including commercially, with an explicit patent grant. The license does not cover the Hekma or Ktesio names; see [TRADEMARK.md](https://github.com/Ktesio/hekma/blob/main/TRADEMARK.md). Releases up to 0.8.1 shipped under an earlier source-available license and keep it.

Hekma is a [Ktesio](https://ktesio.com) project, developed in the open at [ktesio.dev](https://ktesio.dev).
