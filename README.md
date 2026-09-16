<p align="center">
  <img src="docs/assets/hemaka-banner.png" alt="Hemaka banner: run AI agents like services — supervise, meter, and budget them" width="100%">
</p>

# Hemaka

[![CI](https://github.com/Ktesio/ktesio/actions/workflows/ci.yml/badge.svg)](https://github.com/Ktesio/ktesio/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/hemaka.svg)](https://crates.io/crates/hemaka)
[![License](https://img.shields.io/badge/license-Ktesio%20NC--Attribution%201.0.0-blue.svg)](LICENSE)

Hemaka is a [Ktesio](https://ktesio.com) project (the OSS branch lives on [ktesio.dev](https://ktesio.dev)).

> **License posture.** Hemaka is a Ktesio project. It is source-available under the Ktesio Noncommercial-Attribution License 1.0.0 — not OSI open source; commercial use requires written permission from the copyright holder.

Hemaka is a Rust CLI and engine that **runs AI agents like services** — supervise their lifecycle, meter real token usage, and enforce dollar budgets. Register any agent, start and stop it under supervision, watch what it actually consumes, and set token and cost ceilings that stop runaway spend the moment they are crossed.

**What "agent" means here.** A third-party program you run on your own machine or server that calls a model on your behalf: a personal agent such as Hermes Agent or OpenClaw, or a coding agent such as OpenCode or GitHub Copilot CLI. Hemaka launches, supervises, meters, and budgets that program as a process. It is not a framework for writing agents, and it is not an agent itself.

## Why Hemaka?

Long-running AI agents are processes that cost money on every call. Hemaka treats them like the services they are:

- **Lifecycle — run agents like services.** `start`, `stop`, `pause`, and `resume` any registered agent through one uniform state machine, with crash detection, a configurable Restart Policy, captured logs, and durable state (one SQLite database) that survives an engine restart or reboot and reconciles orphaned processes honestly.
- **Metering — real token usage.** Every registered agent declares a Metering Source, and the engine records real per-run and cumulative token totals into a durable Usage Ledger. Usage is either **self-reported** by the agent or **engine-observed** through a loopback proxy, so governance never depends on the agent's cooperation.
- **Budgets & cost control — ceilings that actually stop spend.** Set per-run and cumulative **token** budgets, and (with a configured Rate) **dollar** cost caps. Each carries a Breach Action — `pause`, `stop`, or `warn` — enforced the instant a ceiling is reached, in the same commit path as the usage that crossed it. Every dollar figure is integer micro-dollars, labeled an estimate.
- **One vocabulary, any agent.** Register a native builtin, or bring your own agent with a small `adapter.toml` manifest that declares how to launch it, its per-OS capabilities, and its metering source. Configure every agent through one layered-TOML config with per-value provenance and `secret:NAME` references that stay masked in Hemaka's surfaces.
- **Embedding — a library, not just a CLI.** The engine is an embeddable Rust library (`hemaka-engine`) with a blocking facade and a subscribe surface, so a host application drives the whole fleet — lifecycle, configuration, budgets, and events — without the CLI. See [Embedding the engine](docs/embedding.md).

## Install

Hemaka ships two command names — `hemaka` and the short alias `maka` — for
macOS, Linux, and Windows. Both run the same CLI: every command below works
under either name.

Install on macOS or Linux:

```bash
curl -fsSL https://cli.hemaka.dev/hemaka/install.sh | sh
```

Install on Windows with PowerShell:

```powershell
irm https://cli.hemaka.dev/hemaka/install.ps1 | iex
```

New macOS and Linux installs prefer Homebrew, then Cargo, then a prebuilt GitHub
Release binary. New Windows installs prefer Cargo, then a prebuilt GitHub Release
binary. The installer preserves an existing install channel when it can.

If you already have Rust, install from crates.io (the `hemaka` package installs the `hemaka` binary):

```bash
cargo install hemaka
```

Or with Homebrew:

```bash
brew install ktesio/tap/hemaka
```

> **Upgrading from `kt` (pre-0.8.0 Ktesio releases)?** Your data directory is
> untouched — Hemaka reads the same location. Run the installer again (or
> `cargo install hemaka --force`, or `brew upgrade ktesio/tap/hemaka`) and see
> the [migration guide](docs/migration.md). The old `kt self-update` cannot
> reach 0.8.0 (the release archives renamed); re-running the installer is the
> documented path.
>
> Upgrading from the even older `imagdy/tap` location? Run `brew untap imagdy/tap && brew install ktesio/tap/hemaka` once — the tap lives in the Ktesio org.

You can also download a release archive from [GitHub Releases](https://github.com/Ktesio/ktesio/releases), unpack it, and place `hemaka` on your `PATH`, or build from source:

```bash
git clone https://github.com/Ktesio/ktesio.git
cd hemaka
cargo install --path .
```

See the [installation guide](docs/installation.md) for update behavior and per-platform notes.

## Quickstart

Register an agent, give it a budget, inspect it, and run it under supervision.

### 1. Describe your agent with a manifest adapter

An agent is registered through an `adapter.toml` that declares how to launch it, its per-OS capabilities, and its metering source. Create a directory `my-agent/` with an `adapter.toml`:

```toml
contract_version = "1.0.0"

[adapter]
kind = "my-agent"
name = "My Agent"

# How the engine launches the agent process. exec must be on PATH (or an
# absolute path); args/env are optional. Replace this with your agent's command.
[lifecycle.start]
exec = "my-agent"
args = ["--serve"]

# A non-empty, per-OS Capability Declaration. "pause" is guaranteed on Unix
# (SIGSTOP) and best-effort on Windows.
[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "best-effort"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

# A viable Metering Source: "self-reported" (the agent emits its own usage) or
# "engine-observed" (the engine meters model traffic through a loopback proxy).
[metering]
source = "self-reported"
```

Register it under a Fleet-unique name:

```bash
hemaka agent register my-agent --manifest ./my-agent
```

Registration validates the manifest first, then creates an isolated Agent Home and prints its path plus the effective (current-OS) Capability Declaration. (To try the flow without writing a manifest, `hemaka agent register demo --kind mock` registers a native builtin — note that `mock` has no launch command, so it cannot be started.)

### 2. Set a budget and a cost cap

Budgets are ordinary layered-config values, inspectable and changeable at any time:

```bash
# Cap cumulative token usage and pause the agent on breach.
hemaka agent config set my-agent budget.tokens.cumulative 500000
hemaka agent config set my-agent budget.breach_action pause

# Optional: price tokens ($/1M) so token usage derives a dollar cost, then cap it.
hemaka agent config set my-agent cost.rate.input 3.00
hemaka agent config set my-agent cost.rate.output 15.00
hemaka agent config set my-agent budget.dollars.cumulative 10.00
```

### 3. Inspect the Fleet

```bash
hemaka agent list            # a table: name, kind, state, restarts, budget, usage
hemaka agent show my-agent   # one instance: capabilities, state, budget, usage, cost, metering source
hemaka agent config get my-agent   # the effective config, with the source layer of each value
```

`hemaka agent list --json` and `hemaka agent show my-agent --json` emit a versioned, machine-readable document. Token totals are the Usage Ledger sums exactly; dollar figures appear only when a Rate is configured and are always labeled estimates.

### 4. Run it under supervision

```bash
hemaka agent start my-agent
hemaka agent pause my-agent      # honest per-OS: guaranteed / best-effort / unsupported
hemaka agent resume my-agent
hemaka agent stop my-agent --timeout 10   # graceful, then a forced kill after the window
```

> **Supervision boundary (current behavior):** a standalone `hemaka agent start` supervises the process only for that command's lifetime and stops it when the command exits; start it with `hemaka agent start --detach` to keep the agent running across commands (the next `hemaka` command re-adopts it; between commands it is not supervised — no crash detection, no budget enforcement, no usage/event delivery). If the engine crashes with a surviving process, the next engine open re-adopts it, detects crashes, and applies the Restart Policy.

## Commands

The agent runner lives under `hemaka agent`. Every command supports `--help`.

| Command | Purpose |
|---------|---------|
| `hemaka agent register` | Register an instance from a native builtin (`--kind`) or an `adapter.toml` manifest adapter (`--manifest`) |
| `hemaka agent list` | List every Agent Instance in the Fleet |
| `hemaka agent show` | Show one instance's capabilities, runtime status, usage, and budget |
| `hemaka agent usage` | Read Usage Ledger totals for one instance, or Fleet-wide |
| `hemaka agent start` / `stop` / `pause` / `resume` | Drive the lifecycle |
| `hemaka agent send` | Send a line of input to a running instance's stdin |
| `hemaka agent logs` | Read an instance's retained output (`--follow` to stream, `--json` emits NDJSON) |
| `hemaka agent remove` | Remove an instance (retain or delete its Agent Home; `--force` if running) |
| `hemaka agent config set` / `get` | Write and read the layered config (validated; `--reveal` un-masks secrets) |
| `hemaka agent memory attach` / `detach` | Attach or detach a Memory Backing (`filesystem` or `native`) |

See the [command reference](docs/commands.md) for arguments, flags, and the unified config keys, and the [exit-code table](docs/commands.md#exit-codes) for the documented numeric codes every command returns.

## Prove your adapter conforms

If you build an adapter (a manifest `adapter.toml` shipped with your agent, or a native adapter crate), the **Conformance Test Kit** proves it honors the Adapter Contract — the same controls, metering honesty, and capability declarations every built-in adapter is held to. Add one dev-dependency and one `#[test]`:

```toml
# Pin a full commit SHA: until the crates publish (story 7-4) a bare `git =`
# dependency floats on this repo's default-branch HEAD, and a breaking
# report-shape change would break your build without you moving. Update the
# pin deliberately.
[dev-dependencies]
# The kit is not published to a registry yet — depend on it by git until
# then (a workspace-relative path like `../hemaka-conformance` only works
# inside this repository; a git dependency works for any third party):
hemaka-conformance = { git = "https://github.com/Ktesio/ktesio", rev = "20ddc204403a5c412e0e3249d4609dd47c30854e" }  # pin a current rev
```

```rust
#[test]
fn my_adapter_conforms() {
    let report = hemaka_conformance::run_mock_conformance(std::path::Path::new("adapter-dir"));
    assert!(report.is_conformant(), "failures = {:?}", report.failures());
}
```

The harness registers your adapter with a fresh engine and returns a machine-readable report (versioned with `schema_version`): one entry per contract section — capability projection, lifecycle (including crash), pause, config mapping, both metering sources, memory delivery, and interaction — each `pass`, `fail` (with the first failure reason), or `not_applicable` (justified from your adapter's own declaration: a self-reported metering source, for example, is never asked for engine-observed proof, and a `pause: unsupported` declaration skips the pause demonstration honestly).

Plainly, four sections exercise **your adapter itself** — capability projection, lifecycle, pause, and (for manifest adapters) config delivery through your declared rules — while the metering, memory, and interaction sections prove the same engine seams through small probe fixtures the harness brings along, so a failing probe never damages your adapter's run. Native builtins register by kind via `run_conformance(&TckAdapter::Native(...))`; `run_mock_conformance` is the manifest-adapter shorthand. The section semantics are documented in [Testing](docs/testing.md#the-conformance-test-kit).

## Documentation

- [Getting started](docs/get-started.md)
- [Installation](docs/installation.md)
- [Command reference](docs/commands.md)
- [Adapter manifest (`adapter.toml`)](docs/manifest.md)
- [Adapter Contract](docs/adapter-contract.md)
- [Embedding the engine (library quickstart)](docs/embedding.md)
- [Architecture](docs/architecture.md)
- [Testing](docs/testing.md)
- [Release process](docs/release-process.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Contributing](CONTRIBUTING.md)

## Project Status

Hemaka is early and moving fast. The lifecycle, layered configuration, secrets, the Usage Ledger, token budgets, dollar cost caps, and engine-observed metering are implemented today — as is the event stream: a host (or any Rust consumer) can subscribe to lifecycle, breach, and usage events through `Engine::subscribe` / `Blocking::subscribe` on the embedded engine. The Hermes Agent adapter ships as a native builtin (`--kind hermes`); OpenCode has been validated against the Adapter Contract on paper, with no shipped adapter yet; any other agent registers through an `adapter.toml` manifest that declares how to launch it and where its usage numbers come from. A supervising daemon (durable cross-invocation supervision) and a richer native adapter surface are on the roadmap.

## License

Hemaka is **source-available**, licensed under the [Ktesio Noncommercial-Attribution License 1.0.0](LICENSE).

- **Noncommercial use is free.** You may use, copy, modify, and share Hemaka for any noncommercial purpose under the terms of the license.
- **Visible credit is required.** Whenever you distribute Hemaka, distribute a modified version of it, use it in your own product or distribution, or operate it to provide functionality to third parties, you must prominently credit the Hemaka project and its author ("Islam Magdy", the copyright holder) in at least one place a reasonable user would readily see — your product's documentation, an "About" or credits screen, or a public README all qualify. Private, internal use that reaches no third party owes no credit. See the Attribution section of the LICENSE file.
- **Commercial use requires a separate license.** Any commercial use needs the prior written permission of the copyright holder, Islam Magdy. To request a commercial license, open an issue or contact the maintainer through the project's official channels.

Based on the PolyForm Noncommercial License 1.0.0 (polyformproject.org), modified: an Attribution condition has been added. This is a custom license, not a PolyForm license.

This is source-available software, not an OSI-approved open source license.

## Contributing

Contributions are welcome under the project's [Contributor License Agreement](CLA.md): anyone can contribute, but you assign copyright in your contribution to the project owner so Hemaka stays under unified ownership. See [CONTRIBUTING.md](CONTRIBUTING.md) and [CLA.md](CLA.md) for details.
