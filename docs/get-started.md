---
title: Getting Started
description: Register an AI agent, give it a budget, inspect it, and run it under supervision — end to end.
---

# Quickstart

This guide runs an agent through Hemaka end to end: register it, budget it, inspect the Fleet, and drive its lifecycle. The agent can be any third-party agent program you can launch as a process: a personal agent such as Hermes Agent or OpenClaw, or a coding agent such as OpenCode or GitHub Copilot CLI.

## Install Hemaka

Install the `hemaka` binary (see the [installation guide](installation.md) for every channel):

```bash
curl -fsSL https://cli.hemaka.dev/hemaka/install.sh | sh
```

Or build from source:

```bash
git clone https://github.com/Ktesio/ktesio.git
cd hemaka
cargo install --path .
```

Verify:

```bash
hemaka --version
hemaka agent --help
```

## Describe your agent with a manifest adapter

Hemaka registers an agent through an **adapter** — either a native builtin (`--kind`) or a **manifest adapter** you supply as an `adapter.toml` (`--manifest`). A manifest declares how to launch the agent, its per-OS capabilities, and its metering source.

Create a directory `my-agent/` containing `adapter.toml`:

```toml
contract_version = "1.0.0"

[adapter]
kind = "my-agent"
name = "My Agent"

# How the engine launches the agent. exec must resolve on PATH (or be absolute);
# args and env are optional. Replace this with your agent's real command.
[lifecycle.start]
exec = "my-agent"
args = ["--serve"]

# A non-empty, per-OS Capability Declaration (linux / macos / windows), each
# "guaranteed", "best-effort", or "unsupported".
[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "best-effort"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

# A viable Metering Source: "self-reported" or "engine-observed".
[metering]
source = "self-reported"
```

See the [adapter manifest reference](manifest.md) for every section and field.

## Register the Agent

```bash
hemaka agent register my-agent --manifest ./my-agent
```

Registration validates the manifest, creates an isolated **Agent Home**, and prints its path plus the effective (current-OS) Capability Declaration. Nothing is written if validation fails.

To try the flow without writing a manifest, register the native builtin:

```bash
hemaka agent register demo --kind mock
```

`mock` is a registration/config fixture — it declares capabilities and a metering source but has **no launch command**, so it cannot be started. Use a manifest adapter to run a real process.

## Set a budget and a cost cap

Budgets and rates are ordinary unified-config values, validated at write time and changeable at any time:

```bash
# Token budget: cap cumulative usage, and pause the agent when it is reached.
hemaka agent config set my-agent budget.tokens.cumulative 500000
hemaka agent config set my-agent budget.breach_action pause

# Optional dollar cost control: price tokens in $/1M, then cap the derived cost.
hemaka agent config set my-agent cost.rate.input 3.00
hemaka agent config set my-agent cost.rate.output 15.00
hemaka agent config set my-agent budget.dollars.cumulative 10.00
```

The Breach Action (`pause`, `stop`, or `warn`) fires the instant a ceiling is reached, on real usage from the Usage Ledger — `warn` records the breach event only and performs no lifecycle transition. A dollar cap set without a Rate is inert until a Rate exists.

## Inspect the fleet

```bash
hemaka agent list                  # name, kind, state, restarts, budget, usage
hemaka agent show my-agent         # capabilities, runtime status, usage, budget, cost, metering source
hemaka agent usage my-agent        # Usage Ledger totals for one instance (or Fleet-wide without a name)
hemaka agent config get my-agent   # the effective config with the source layer of each value
```

Add `--json` to `list` or `show` for a versioned, machine-readable document. Token totals equal the Usage Ledger exactly; dollar figures appear only when a Rate is configured and are always labeled estimates.

## Drive the lifecycle

```bash
hemaka agent start my-agent
hemaka agent pause my-agent
hemaka agent resume my-agent
hemaka agent stop my-agent --timeout 10
```

`pause` is honest per-OS: a guaranteed pause suspends the process, a best-effort pause proceeds cooperatively and prints a visible note, and an unsupported pause fails fast quoting the Capability Declaration. `stop` requests a graceful shutdown and escalates to a forced kill after the window (`--timeout`, default 30s).

> **Supervision boundary:** a standalone `hemaka agent start` supervises the process only for that command's lifetime and stops it when the command exits. To keep the agent running across commands, start it with `hemaka agent start --detach` — the agent survives the command's exit and the next `hemaka` command re-adopts it; between commands it is *not* supervised (no crash detection, no budget enforcement, no usage/event delivery — supervision is command-scoped). If the engine crashes with a surviving process, the next engine open re-adopts it, detects crashes, and applies the Restart Policy.

## Manage secrets

Reference secrets indirectly with a `secret:NAME` value — the reference is stored, and the real value is resolved from the environment (then the engine secrets file) at start and delivered to the agent, while staying masked in `hemaka agent config get`, snapshots, logs, and events:

```bash
hemaka agent config set my-agent agent.api_key secret:OPENAI_KEY
hemaka agent config get my-agent               # shows secret:**** for that key
hemaka agent config get my-agent --reveal      # the sole explicit un-mask
```

## Remove an agent

```bash
hemaka agent remove my-agent            # keeps the Agent Home by default
hemaka agent remove my-agent --delete   # also deletes the Agent Home
```

## Next steps

- Read the [command reference](commands.md).
- Learn the [adapter manifest format](manifest.md).
- Check [troubleshooting](troubleshooting.md) for common setup and PATH issues.
