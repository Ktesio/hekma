---
title: opencode
description: 'opencode under Hekma — run-verified via its native ACP server (opencode acp), with honest context-grain-only metering.'
---

# opencode

opencode (anomalyco) ships a native ACP server since v2 (its `acp`
subcommand), so it runs under Hekma's built-in `--kind acp` adapter with no
per-agent adapter work. No login is needed for the default free models; a
paid model requires `opencode auth login` first — a logged-out agent refuses
at the handshake with a JSON-RPC error code the engine surfaces.

## Register

```bash
hekma agent config set my-opencode acp.command "opencode"
hekma agent config set my-opencode acp.args "acp"
hekma agent register my-opencode --kind acp
hekma agent start my-opencode
```

## Honesty table

The `pause` and `interaction` levels below are the `acp` builtin's own
code-declared Capability Declaration — identical for every ACP agent and
proven by the conformance kit against a scripted contract twin; they are not
per-agent promises. The metering rows are what the pinned real-agent run
actually demonstrated.

| Surface | Value | Evidence class |
|---------|-------|----------------|
| `pause` | `guaranteed` (linux), `guaranteed` (macos), `best-effort` (windows) | `contract-validated` |
| `interaction` | `guaranteed` (all OSes) | `contract-validated` |
| Metering source | `self-reported` | `contract-validated` |
| Billing-grade tokens | `—` — no billing-grade usage in the ledger — tiers attempted: observed: not-configured; sentinel: no-lines-seen; context usage reported (ACP usage_update is context-grain, never billed). opencode reports per-turn token counts inside its `session/prompt` RESULT as well as a context-grain `usage_update` — both outside the v1 billing tiers, so they surface honestly and bill nothing | `run-verified` |
| Context usage | the agent's `usage_update` surfaces under `acp_context_usage` — context-grain, never billed | `run-verified` |

## Validation status

- **Upstream stability:** stable.
- **Evidence class:** `run-verified` — registered, started (handshake
  against its rich `initialize`/`session/new` shapes), one full prompt turn
  completed with the reply retained, context-grain metering surfaced,
  stopped clean. Its dialect extras (`authMethods`, `configOptions`,
  `available_commands_update`) are pinned against the contract twin, so
  tolerance does not depend on having the binary.
- **Validated against:** `v2.0.28` (2026-10-16). The evidence, the
  real-agent smoke tests, and the pre-v2 manifest characterization are
  recorded in [Supported Agents](../agents.md).

## Isolation

Under the `acp` kind, opencode's data/config roots are expected to follow
its XDG levers as before. Set `XDG_DATA_HOME`/`XDG_CONFIG_HOME` in the
instance's env config for per-instance isolation — per-instance isolated
roots inside each Agent Home are the recommended normative posture in the
[Adapter Contract](../adapter-contract.md#configuration-delivery).

## PATH requirement

The launch's `exec` is the bare word `opencode`, resolved through the
operator's `PATH` at start. Hekma does not bundle, install, or pin the
opencode binary — you install it (per opencode's own docs), keep it on the
`PATH` of the environment `hekma` runs in, and `hekma agent start` resolves
it like any other program. A start whose `opencode` cannot resolve fails
with the engine's launch-failure diagnostic naming the executable.

## Re-validation duty

opencode ships multiple releases per week; the pin above is a
stale-the-moment-it-is-written snapshot by design. Re-validate against your
installed version before relying on agent-specific behavior, and move the
pin forward with fresh evidence — [the re-validation
duty](../agents.md#the-re-validation-duty) states the full obligation,
including the contract's honor-system duties that no automated section can
prove.
