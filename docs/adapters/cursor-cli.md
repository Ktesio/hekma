---
title: Cursor CLI
description: 'Cursor CLI under Hekma — ACP via its agent acp subcommand (a hidden/advanced surface); characterized from upstream docs, not run-verified here.'
---

# Cursor CLI

Cursor's CLI exposes an ACP mode through its `agent` binary's `acp`
subcommand, so it runs under Hekma's built-in `--kind acp` adapter. Upstream
documents the mode as a hidden/advanced command; this entry is characterized
from upstream documentation and has not been run-verified here.

## Register

```bash
hekma agent config set my-cursor acp.command "agent"
hekma agent config set my-cursor acp.args "acp"
hekma agent register my-cursor --kind acp
hekma agent start my-cursor
```

> The exec word `agent` is generic and collision-prone — any other `agent`
> binary earlier on `PATH` would be launched instead. Confirm what resolves
> (`command -v agent`) before `start`, or set `acp.command` to the CLI's
> absolute path on your machine.

## Honesty table

The `pause` and `interaction` levels below are the `acp` builtin's own
code-declared Capability Declaration — identical for every ACP agent and
proven by the conformance kit against a scripted contract twin; they are not
per-agent promises. The metering rows are characterized from upstream
documentation, not run-verified here.

| Surface | Value | Evidence class |
|---------|-------|----------------|
| `pause` | `guaranteed` (linux), `guaranteed` (macos), `best-effort` (windows) | `contract-validated` |
| `interaction` | `guaranteed` (all OSes) | `contract-validated` |
| Metering source | `self-reported` | `contract-validated` |
| Billing-grade tokens | `—` — no billing-grade usage in the ledger — tiers attempted: observed: not-configured; sentinel: no-lines-seen; no context usage reported (ACP usage_update is context-grain, never billed). The backend is subscription-backed and no base-URL override is documented upstream | `characterized` |
| Context usage | an agent-reported `usage_update` would surface under `acp_context_usage` — context-grain, never billed; whether this agent reports one is unverified here | `characterized` |

## Validation status

- **Upstream stability:** experimental (documented as a hidden/advanced
  command; it can change without a deprecation cycle).
- **Evidence class:** `characterized` — upstream documentation, dated
  2026-10-23, NOT run-verified here.
- **Validated against:** nothing pinned.

## PATH requirement

The launch's `exec` is the bare word `agent`, resolved through the
operator's `PATH` at start. Hekma does not bundle, install, or pin the
Cursor CLI binary — you install it (per Cursor's own docs), keep it on the
`PATH` of the environment `hekma` runs in, and `hekma agent start` resolves
it like any other program. A start whose `agent` cannot resolve fails with
the engine's launch-failure diagnostic naming the executable.

## Re-validation duty

A hidden/advanced upstream surface can change in any release;
every characterization here is a stale-the-moment-it-is-written snapshot by
design. Re-validate against your installed version before relying on
agent-specific behavior — [the re-validation
duty](../agents.md#the-re-validation-duty) states the full obligation,
including the contract's honor-system duties that no automated section can
prove.
