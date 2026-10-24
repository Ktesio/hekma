---
title: Gemini CLI
description: 'Gemini CLI under Hekma — ACP via its documented --experimental-acp flag; smoke-listed, characterized, not run-verified with a pin.'
---

# Gemini CLI

Google's Gemini CLI speaks ACP when started with its ACP-mode flag, so it
runs under Hekma's built-in `--kind acp` adapter. It needs a logged-in CLI
(or API key) before it will open a session — a logged-out `session/new`
answers JSON-RPC `-32000`, and the engine surfaces the code, never the
agent's message.

## Register

```bash
hekma agent config set my-gemini acp.command "gemini"
hekma agent config set my-gemini acp.args "--experimental-acp"
hekma agent register my-gemini --kind acp
hekma agent start my-gemini
```

The `--experimental-acp` spelling is the long-documented IDE-integration
form; newer builds also accept the shortened `--acp`.

## Honesty table

The `pause` and `interaction` levels below are the `acp` builtin's own
code-declared Capability Declaration — identical for every ACP agent and
proven by the conformance kit against a scripted contract twin; they are not
per-agent promises. The metering rows are characterized, not run-verified
with a pin.

| Surface | Value | Evidence class |
|---------|-------|----------------|
| `pause` | `guaranteed` (linux), `guaranteed` (macos), `best-effort` (windows) | `contract-validated` |
| `interaction` | `guaranteed` (all OSes) | `contract-validated` |
| Metering source | `self-reported` | `contract-validated` |
| Billing-grade tokens | `—` — no billing-grade usage in the ledger — tiers attempted: observed: not-configured; sentinel: no-lines-seen; no context usage reported (ACP usage_update is context-grain, never billed). No sentinel emission and no base-URL override is documented upstream | `characterized` |
| Context usage | an agent-reported `usage_update` would surface under `acp_context_usage` — context-grain, never billed; whether this agent reports one is unverified here | `characterized` |

## Validation status

- **Upstream stability:** experimental (the ACP mode's own flag spelling).
- **Evidence class:** `characterized` — repo-documented launch shape, and
  the agent is smoke-listed: the repo ships a skip-unless-present real-agent
  smoke that runs wherever the `gemini` binary exists (an honest no-op in
  CI's network-free sandbox). It has **no** version pin and no run-verified
  turn on record here.
- **Validated against:** nothing pinned — see
  [Supported Agents](../agents.md) for what that would take.

## PATH requirement

The launch's `exec` is the bare word `gemini`, resolved through the
operator's `PATH` at start. Hekma does not bundle, install, or pin the
Gemini CLI binary — you install it (per its own docs), keep it on the `PATH`
of the environment `hekma` runs in, and `hekma agent start` resolves it like
any other program. A start whose `gemini` cannot resolve fails with the
engine's launch-failure diagnostic naming the executable.

## Re-validation duty

Gemini CLI moves quickly and its ACP mode carries the experimental flag
spelling; every characterization here is a stale-the-moment-it-is-written
snapshot by design. Re-validate against your installed version before
relying on agent-specific behavior — [the re-validation
duty](../agents.md#the-re-validation-duty) states the full obligation,
including the contract's honor-system duties that no automated section can
prove.
