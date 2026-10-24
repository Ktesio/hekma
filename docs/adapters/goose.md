---
title: goose
description: 'goose under Hekma — ACP via its goose acp subcommand (experimental upstream); characterized from upstream docs, not run-verified here.'
---

# goose

Block's goose exposes an ACP mode through its `goose acp` subcommand, so it
runs under Hekma's built-in `--kind acp` adapter. The mode is experimental
upstream; this entry is characterized from upstream documentation and has
not been run-verified here.

## Register

```bash
hekma agent config set my-goose acp.command "goose"
hekma agent config set my-goose acp.args "acp"
hekma agent register my-goose --kind acp
hekma agent start my-goose
```

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
| Billing-grade tokens | `—` — no billing-grade usage in the ledger — tiers attempted: observed: not-configured; sentinel: no-lines-seen; no context usage reported (ACP usage_update is context-grain, never billed). No sentinel emission is documented upstream; the engine-observed tier is plausible (goose providers take base URLs) but unproven — wiring it is an experiment, not a documented integration | `characterized` |
| Context usage | an agent-reported `usage_update` would surface under `acp_context_usage` — context-grain, never billed; whether this agent reports one is unverified here | `characterized` |

## Validation status

- **Upstream stability:** experimental (the ACP mode is an experimental
  upstream feature).
- **Evidence class:** `characterized` — upstream documentation, dated
  2026-10-23, NOT run-verified here.
- **Validated against:** nothing pinned.

## PATH requirement

The launch's `exec` is the bare word `goose`, resolved through the
operator's `PATH` at start. Hekma does not bundle, install, or pin the goose
binary — you install it (per goose's own docs), keep it on the `PATH` of the
environment `hekma` runs in, and `hekma agent start` resolves it like any
other program. A start whose `goose` cannot resolve fails with the engine's
launch-failure diagnostic naming the executable.

## Re-validation duty

An experimental upstream feature can change or disappear in any release;
every characterization here is a stale-the-moment-it-is-written snapshot by
design. Re-validate against your installed version before relying on
agent-specific behavior — [the re-validation
duty](../agents.md#the-re-validation-duty) states the full obligation,
including the contract's honor-system duties that no automated section can
prove.
