---
title: Adapter Gallery
description: The conformance-validated index of agents that run under Hekma today — per-capability honesty tables in the Capability Declaration vocabulary, upstream stability, and the re-validation duty per entry.
---

# Adapter Gallery

This gallery is the validated index of which agents run under Hekma today:
one page per agent, each with an honesty table in the Capability Declaration
vocabulary, an explicit evidence class for every claim, and the re-validation
duty stated per entry. It exists because agent upstreams move fast and an
adapter's honesty is only as good as its last validation — a claim without a
dated evidence class is not a claim this gallery makes.

Nine of the ten entries register through the built-in `--kind acp` adapter
(any Agent Client Protocol v1 agent over stdio); Aider integrates through a
manifest `adapter.toml`. The shipped manifest is validated in CI by the
[Conformance Test Kit](adapter-contract.md#what-is-checked-versus-honored-the-tcks-honest-reach)
against a bundled fake whose exec is shadowed onto `PATH` — CI never launches
a real agent, and an entry claiming more than its validation proves fails the
build.

## The entries

| Agent | Integration | Upstream status | Evidence class |
|-------|-------------|-----------------|----------------|
| [Hermes Agent](adapters/hermes.md) | `--kind acp` (`hermes-acp`) | stable | `run-verified` |
| [opencode](adapters/opencode.md) | `--kind acp` (`opencode acp`) | stable | `run-verified` |
| [Gemini CLI](adapters/gemini-cli.md) | `--kind acp` (`--experimental-acp`) | experimental | `characterized` |
| [GitHub Copilot CLI](adapters/copilot-cli.md) | `--kind acp` (`--acp --stdio`) | preview | `characterized` |
| [Cursor CLI](adapters/cursor-cli.md) | `--kind acp` (`agent acp`) | experimental | `characterized` |
| [goose](adapters/goose.md) | `--kind acp` (`goose acp`) | experimental | `characterized` |
| [Claude Code](adapters/claude-code.md) | `--kind acp` via a third-party adapter | experimental (third-party adapter) | `characterized` |
| [Codex](adapters/codex.md) | `--kind acp` via a third-party adapter | experimental (third-party adapter) | `characterized` |
| [Crush](adapters/crush.md) | `--kind acp` via a third-party adapter (unofficial) | experimental (third-party adapter, unofficial) | `characterized` |
| [Aider](adapters/aider.md) | manifest (`adapter.toml`) | stable | `characterized` |

## The vocabulary, stated once

Every table cell in this gallery uses one vocabulary, verbatim:

**Capability levels** (per OS, the [Capability Declaration](adapter-contract.md#capability-declarations)
vocabulary):

- `guaranteed` — the engine may rely on it.
- `best-effort` — surfaced honestly (a qualifier cause on the transition,
  e.g. `pause-best-effort`), never silently treated as guaranteed.
- `unsupported` — the command fails fast, quoting the declaration; nothing
  is faked.

**Metering sources** (the [Adapter Contract's](adapter-contract.md#metering)
two-tier vocabulary): `self-reported` (the agent's usage accounting reaches
the engine as `KTESIO_USAGE {json}` sentinel lines — under `acp`, on the
agent's **stderr**, its stdout being the protocol stream) or
`engine-observed` (the engine meters the agent's model traffic through a
per-instance loopback proxy — no agent cooperation needed).

**Evidence classes** — how a claim in this gallery came to be believed:

- `run-verified` — launched, supervised, and driven against the real agent,
  version-pinned. Pins live in [Supported Agents](agents.md) and are repeated
  on the entry pages that have one.
- `contract-validated` — proven by the Conformance Test Kit against a
  bundled fake (the machine-checked half of the contract); it proves the
  declaration's machinery, never the real agent's behavior.
- `characterized` — taken from upstream documentation, **dated**, and NOT
  run-verified here. These entries are starting points for your own
  validation, not endorsements.

**The honest absence token.** Where no metering tier yields billing-grade
usage, the token/cost cells show `—` — never a blank, a fabricated figure,
or an implied zero — and the tiers-attempted notice names what was tried.
For an `acp` instance with nothing wired, the notice reads:

> no billing-grade usage in the ledger — tiers attempted: observed:
> not-configured; sentinel: no-lines-seen; no context usage reported (ACP
> usage_update is context-grain, never billed)

The states are computed from your instance's actual run; an agent that
reports a context window flips the last clause to `context usage reported`,
and the figure surfaces under `acp_context_usage` — context-grain, never
billed.

## Two rules every entry carries

**The PATH requirement.** Every published exec is a bare program name,
resolved through the operator's `PATH` at start (the [manifest
reference](manifest.md#lifecycle)'s own resolution rule). Hekma does not
bundle, install, or pin any agent binary — you install it per its own docs
and keep it on the `PATH` of the environment `hekma` runs in. A start whose
executable cannot resolve fails with the engine's launch-failure diagnostic
naming the executable.

**The re-validation duty.** Every version pin and every dated
characterization here is a stale-the-moment-it-is-written snapshot by
design: pins make validation auditable, not permanent. Whoever ships a
release leaning on an entry — or relies on agent-specific behavior —
re-validates against the installed version and moves the record forward.
See [the re-validation duty](agents.md#the-re-validation-duty) for the full
statement, including the contract's honor-system duties, which are exactly
the parts no automated section can prove.
