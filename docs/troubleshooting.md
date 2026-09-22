---
title: Troubleshooting
description: Common Hekma install, registration, config, and lifecycle issues with practical fixes.
---

# Troubleshooting

## Adapter Manifest Not Found or Invalid

`hekma agent register --manifest <path>` reports the exact problem and writes nothing when a manifest is missing or invalid.

- **Not found** — pass a directory containing `adapter.toml`, or the path to the `adapter.toml` file itself.
- **Invalid** — the error names the first missing or invalid mandatory section (`contract_version`, `[adapter]`, `[lifecycle.start]`, `[capabilities]`, or `[metering]`) or the unknown key it rejected.

See the [adapter manifest reference](manifest.md) for the required shape.

## "Incompatible Adapter Contract" at Registration

`hekma agent register --manifest <path>` refuses the manifest with a message like:

```text
incompatible adapter contract: manifest declares 2.1.0, engine speaks 1.0.0 — compatible iff the major versions match (contract v1 policy, docs/adapter-contract.md#versioning)
```

Since contract v1 the engine **negotiates**: a manifest whose `contract_version` **major** differs from the engine's does not load. The fix belongs to the adapter author, not the CLI: set `contract_version = "1.0.0"` in the manifest (strict `X.Y.Z` — no `v` prefix, no partials; prerelease suffixes such as `1.0.0-rc.1` parse and negotiate by major). Pre-v1 `0.x` values are not grandfathered. The versioning and deprecation policy is at [the Adapter Contract page](adapter-contract.md#versioning).

Related: `hekma agent memory attach <name> --kind <kind> --json` and `hekma agent memory detach <name> --json` emit versioned documents (`schema_version: 1`) whose key-sets are frozen compatibility surfaces — if one stops parsing for you after an upgrade, an unannounced wire change has occurred; check the release notes.

## Agent Won't Start ("no launch command")

The native `mock` kind is a registration/config fixture with no launch command, so `hekma agent start` fails for it:

```text
native adapter kind 'mock' has no launch command; supply a manifest adapter
```

Register a **manifest adapter** whose `[lifecycle.start]` declares a real `exec` to start a process.

## Hermes Won't Launch ("command not found" / immediate failure)

The `hermes` builtin launches the real Hermes gateway by the bare executable name `hermes`, resolved through the `PATH` of the environment `hekma` runs in — Hekma does not bundle or install it.

- **`hermes: command not found` (a launch failure naming the executable)** — install Hermes and confirm a plain `hermes --version` works in the same shell/account `hekma` runs under; a `hekma` started from a different context may see a different `PATH`.
- **Starts, then lands `failed`** — read `hekma agent logs <name>` for the gateway's own startup error (port conflict, or a Hermes profile already supervised by its own OS service; the declared launch is the foreground `gateway run --external-supervisor`, so stop the service-managed gateway first).
- **Behavior drift after a Hermes upgrade** — check [the supported-agents page](agents.md) validation pin and its re-validation duty before trusting lifecycle/metering behavior.

## Agent Shows `failed` After Starting

A standalone `hekma agent start` supervises the process only for that command's lifetime and stops it when the command exits. A later, separate `hekma agent list` then reports the instance as `failed` because the supervised process is gone.

This is expected for the plain start. If you want the agent to keep running across commands, start it with `hekma agent start --detach`: the agent survives the command's exit and the next `hekma` command re-adopts it. Between commands a detached agent is *not* supervised — no crash detection, no budget enforcement, no usage/event delivery — so if the agent dies in that window, the next command reconciles the row honestly to `failed` instead of restarting it. If the engine crashes with a surviving process, the next engine open re-adopts it, detects crashes, and applies the Restart Policy. (An `acp` instance cannot start detached at all — its transport is a pair of pipes to the starting command — see the ACP section below.)

## ACP Agents (`--kind acp`): Failure Modes

The `acp` kind supervises an Agent Client Protocol v1 agent over its stdio; these are the failure surfaces specific to it.

- **`start` fails: the agent countered a protocol version** — the engine speaks ACP v1 only (the tolerated version set is `{1}`), and an agent whose `initialize` response counters a version outside that set is refused: the connection closes and the instance lands `failed` with a surfaced reason naming the version negotiation — no traffic is exchanged past the handshake. Fix the agent (upgrade to an ACP v1-speaking build) or its launch configuration.
- **`start` fails: cannot resolve or launch the agent** — exactly like any other kind, `acp.command` is resolved through the `PATH` of the environment `hekma` runs in. A `command not found` launch failure names the executable; a missing `acp.command` is refused before anything spawns, naming both `acp.command` and `acp.args`. Also expected: `start --detach` on an `acp` instance always lands `failed` — the ACP transport is a pair of pipes held by the starting command, which detach deliberately severs.
- **A malformed line on the agent's stdout is skipped, not fatal** — the agent's stdout must be pure ACP (one JSON-RPC message per line). A line that does not parse is counted and surfaced as a diagnostic (`… malformed line #N … was skipped; … the stream continues`), and the turn goes on: one bad line never fails a turn or kills the connection. If your agent wraps its stdout with banner output or logs, fix its launch (quiet flags, or an adapter wrapper) — the diagnostics tell you how often it is happening.
- **`send` refused: "already has an ACP turn in flight" (exit code 4)** — ACP serializes turns per session. The second prompt is refused and the FIRST turn is unaffected; wait for its stop reason in `hekma agent logs <name>`, or `hekma agent stop <name>` (which cancels the in-flight turn first). The refusal clears on its own once the turn completes — no restart needed.
- **`send` fails on a re-adopted instance** — the ACP transport is a pair of pipes that died with the engine that spawned the agent, so an adopted `acp` instance holds no live connection: `send` is refused until the instance is stopped and started again. The adoption note on stderr says so, and — since the session id is persisted — the next `start` resumes the previous ACP session via `session/load` when the agent supports it (with an honest note when it does not).
- **Token/cost cells show `—` ("no billing-grade usage in the ledger — tiers attempted: …")** — this is the honest not-available marker, not a metering bug. An ACP agent's `usage_update` notifications report its session **context** window, which Hekma surfaces (the `acp_context_usage` field / the show row) but never bills. Billing-grade tokens arrive only when (a) the agent cooperatively writes `KTESIO_USAGE {json}` sentinel lines — under `acp` on its **stderr**, its stdout being the protocol stream — or (b) you opt the instance into the engine-observed channel (`metering.upstream_base_url`) and the agent honors a base-URL override. The notice names exactly which tiers were attempted and whether context usage was reported; see [the command reference](commands.md#hekma-agent-register-name---kind-kind---manifest-path).

## `start --detach` Refused ("cannot be started with --detach")

A detached start of an `engine-observed` instance is refused before anything changes: the engine-observed channel's loopback forward listener lives inside the starting command, so a detached start would leave the agent pointing at a listener that dies with the command — its model calls would then hit a dead port. Start the instance without `--detach` (the in-command supervision keeps the listener alive), or switch the adapter's metering source to `self-reported` if the instance must detach. This failure exits with code 5 and changes no state.

## Invalid Lifecycle Transition

Commands are rejected uniformly when they don't apply to the current state (for example, `stop` on an instance that is `registered` or `failed`):

```text
cannot stop an Agent Instance while it is 'registered'
```

Check the current state with `hekma agent list` or `hekma agent show <name>`, then issue a valid command. To remove a **running** instance, pass `--force`.

## Config Key Rejected

`hekma agent config set` validates at write time and changes nothing when a key is rejected. An unknown key outside the `agent.*` pass-through namespace is refused with the nearest valid key suggested:

- Use a known unified key (see [Unified Config Keys](commands.md#unified-config-keys)).
- Or put agent-native extras under the `agent.*` namespace, e.g. `hekma agent config set demo agent.temperature 0.2`.

Budget and rate values are validated too: token budgets must parse as integers, and rates/caps must be dollar strings (e.g. `3.00`).

## A Secret Won't Resolve at Start

A `secret:NAME` value is resolved at start from the process environment first, then the engine secrets file at `<state base>/secrets.toml`. If neither provides it, the start is rejected with an error naming the `NAME` and the resolvers tried (never the value). Export the variable or add it to the secrets file, then start again.

On Unix the secrets file must be mode `0600` (owner-only); a group- or world-accessible file is refused with a `chmod 600` remediation.

## Installer Cannot Find `hekma` After Installing

When the installer uses a prebuilt binary, it installs into the detected manual
install directory, `KTESIO_INSTALL_DIR`, or a user-local default directory. If
that directory is not on `PATH`, the installer prints a warning with the exact
directory to add.

Run a dry run to see the selected path without installing:

```bash
curl -fsSL https://cli.ktesio.dev/hekma/install.sh | KTESIO_INSTALL_DRY_RUN=1 sh
```

Then either add the printed directory to `PATH` or choose an existing directory:

```bash
curl -fsSL https://cli.ktesio.dev/hekma/install.sh | KTESIO_INSTALL_DIR="$HOME/.local/bin" sh
```

## Installer Reports an Unsupported OS or Architecture

The prebuilt binary fallback supports macOS Intel, macOS Apple Silicon, Linux
x64, and Windows x64. Other platforms should install with Cargo:

```bash
cargo install hekma --force
```

If Cargo is unavailable, install Rust from [rustup](https://rustup.rs/) first.

## Installer Checksum Verification Fails

The binary installer downloads both the release archive and its `.sha256` file
from GitHub Releases. A checksum mismatch usually means the download was
interrupted, cached incorrectly, or replaced by a network proxy.

Retry the installer. If the error repeats, download the archive and checksum
from [GitHub Releases](https://github.com/Ktesio/hekma/releases) directly and
compare them locally before installing.

## Installer Refuses to Overwrite `hekma`

The installer checks `hekma --version` before replacing an existing `hekma` command.
If the command is not Hekma, the installer stops rather than overwrite another
tool with the same name.

Choose a different install directory and make sure it appears before the other
`hekma` command on `PATH`, or remove the conflicting command if it is no longer
needed.

## Update Check Is Unavailable or Unwanted

Hekma checks GitHub Releases through an hourly cache before running subcommands.
Network failures, cache write failures, and unexpected release responses are
ignored so the requested command can continue.

If you do not want automatic update checks, run commands with:

```bash
KTESIO_NO_UPDATE_CHECK=1 hekma agent list
```

Hekma also skips automatic update checks when `CI=true`.

## Self Update Fails

`hekma self-update` is an explicit update action, so it reports failures instead of
ignoring them.

For Homebrew or Cargo installs, re-run the underlying package manager command to
see full diagnostics:

```bash
brew upgrade ktesio/tap/hekma
cargo install hekma --force
```

For manual installs, Hekma downloads the latest release archive and its
`.sha256` file from GitHub Releases. Retry the command if the download was
interrupted. If checksum verification keeps failing, download the archive and
checksum from [GitHub Releases](https://github.com/Ktesio/hekma/releases) and
compare them locally before replacing the binary.

If your platform does not have a prebuilt release archive, install with Cargo:

```bash
cargo install hekma --force
```

## Usage Totals Stay Zero

`hekma agent usage <name>` (or the usage columns in `list`/`show`) reporting all zeros means no usage was recorded — check the Metering Source:

- **Self-reported** — the agent must emit `KTESIO_USAGE {json}` sentinel lines on its stdout (e.g. `KTESIO_USAGE {"sequence": 0, "input_tokens": 128, "output_tokens": 512}`). Check they are actually reaching stdout: run `hekma agent logs <name>` and look for the lines. A malformed JSON payload is silently dropped as a diagnostic, and stdout that is redirected or wrapped by the agent's own tooling may never reach the captured stream. Under the `acp` kind the sentinel channel is the agent's **stderr** instead — its stdout is the ACP protocol stream — so look for the lines on the stderr side of the log.
- **Engine-observed** — the engine meters only traffic pointed at its loopback proxy. Verify the config mapping that points the agent's OpenAI-compatible `base_url` at the engine-injected `metering.base_url` is declared in the manifest, and that `metering.upstream_base_url` names the real provider endpoint (see [Unified Config Keys](commands.md#unified-config-keys)).

An instance that has never started also reports zeros — that is expected.

## Release Workflow Did Not Update Docs

The tag workflow publishes the GitHub Release first, then opens a pull request for `CHANGELOG.md` and `docs/RELEASE_NOTES.md`.

Check the release workflow logs and open pull requests for a branch named like:

```text
release-docs/<tag>
```
