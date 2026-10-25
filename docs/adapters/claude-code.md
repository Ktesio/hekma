---
title: Claude Code
description: 'Claude Code under Hekma — through a third-party ACP adapter binary; Hekma ships no such adapter, and the supply-chain decision is the operator''s.'
---

# Claude Code

Anthropic's Claude Code has no native ACP mode; it integrates through a
third-party ACP adapter binary that wraps the Claude Code SDK and speaks the
protocol on its behalf. Hekma ships no such adapter — treat any third-party
one as its own supply-chain decision (see
[Supported Agents](../agents.md#acp-agents---kind-acp)).

## Register

The adapter binary below is the third-party wrapper, not Claude Code itself
and not anything Hekma distributes:

```bash
hekma agent config set my-claude-code acp.command "claude-code-acp"
hekma agent register my-claude-code --kind acp
hekma agent start my-claude-code
```

Pass whatever additional arguments your chosen adapter documents via
`acp.args`.

## Honesty table

The `pause` and `interaction` levels below are the `acp` builtin's own
code-declared Capability Declaration — identical for every ACP agent and
proven by the conformance kit against a scripted contract twin; they are not
per-agent promises. The metering rows are characterized, not run-verified
here.

| Surface | Value | Evidence class |
|---------|-------|----------------|
| `pause` | `guaranteed` (linux), `guaranteed` (macos), `best-effort` (windows) | `contract-validated` |
| `interaction` | `guaranteed` (all OSes) | `contract-validated` |
| Metering source | `self-reported` | `contract-validated` |
| Billing-grade tokens | `—` — no billing-grade usage in the ledger — tiers attempted: observed: not-configured; sentinel: no-lines-seen; no context usage reported (ACP usage_update is context-grain, never billed). No sentinel emission is documented upstream; the engine-observed tier is plausible via an `ANTHROPIC_BASE_URL`-style override — a hypothesis, unproven here | `characterized` |
| Context usage | an agent-reported `usage_update` would surface under `acp_context_usage` — context-grain, never billed; whether the adapter reports one is unverified here | `characterized` |

## Validation status

- **Upstream stability:** third-party adapter (the ACP surface is an
  independently maintained wrapper around Claude Code's SDK, outside
  Anthropic's own release cycle and outside this repository's).
- **Evidence class:** `characterized` — repo-documented integration kind,
  dated 2026-10-23, NOT run-verified here.
- **Validated against:** nothing pinned.

## PATH requirement

The launch's `exec` is the bare word `claude-code-acp`, resolved through the
operator's `PATH` at start. Hekma does not bundle, install, or pin the
adapter (or Claude Code) — you install whichever adapter you choose, vet it
yourself, keep it on the `PATH` of the environment `hekma` runs in, and
`hekma agent start` resolves it like any other program. A start whose
`claude-code-acp` cannot resolve fails with the engine's launch-failure
diagnostic naming the executable.

## Re-validation duty

A third-party adapter can drift independently of both Claude Code and this
repository; every characterization here is a stale-the-moment-it-is-written
snapshot by design. Re-validate your adapter + Claude Code combination
before relying on agent-specific behavior — [the re-validation
duty](../agents.md#the-re-validation-duty) states the full obligation,
including the contract's honor-system duties that no automated section can
prove.
