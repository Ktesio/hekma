---
title: 'Hermes Agent'
description: 'Hermes Agent under Hekma — run-verified via its native hermes-acp entry point, with billing-grade self-reported metering.'
---

# Hermes Agent

Hermes Agent (NousResearch) speaks the Agent Client Protocol natively via its
`hermes-acp` entry point, so it runs under Hekma's built-in `--kind acp`
adapter with no per-agent adapter work — and it is the one gallery entry
whose billing-grade metering tier yields out of the box.

## Register

```bash
hekma agent config set my-hermes acp.command "hermes-acp"
hekma agent register my-hermes --kind acp
hekma agent start my-hermes
```

No `acp.args` are needed. The deprecated `hermes` builtin kind targets the
same agent through a fixed gateway launch; new registrations should use
`--kind acp` — see the [deprecation notice](../agents.md#hermes---kind-hermes)
for the migration parity (metering and memory delivery both carry over).

## Honesty table

The `pause` and `interaction` levels below are the `acp` builtin's own
code-declared Capability Declaration — identical for every ACP agent and
proven by the conformance kit against a scripted contract twin; they are not
per-agent promises. The metering rows are what the pinned real-agent runs
actually demonstrated.

| Surface | Value | Evidence class |
|---------|-------|----------------|
| `pause` | `guaranteed` (linux), `guaranteed` (macos), `best-effort` (windows) | `contract-validated` |
| `interaction` | `guaranteed` (all OSes) | `contract-validated` |
| Metering source | `self-reported` | `run-verified` |
| Billing-grade tokens | the `self-reported` sentinel rides the agent's **stderr** (`KTESIO_USAGE {json}` lines; cached tokens included) — the only gallery entry whose sentinel tier yields | `run-verified` |
| Context usage | the agent's `usage_update` surfaces under `acp_context_usage` — context-grain, never billed | `run-verified` |

## Validation status

- **Upstream stability:** stable.
- **Evidence class:** `run-verified` — registered, started (handshake +
  session), one full prompt turn completed with the reply retained, metered
  context-grain, stopped clean.
- **Validated against:** `v0.21.7+500.g8f81791` (2026-10-16). The evidence
  and the real-agent smoke tests that carry it are recorded in
  [Supported Agents](../agents.md).

## PATH requirement

The launch's `exec` is the bare word `hermes-acp`, resolved through the
operator's `PATH` at start. Hekma does not bundle, install, or pin the
Hermes binary — you install it (per Hermes' own docs), keep it on the `PATH`
of the environment `hekma` runs in, and `hekma agent start` resolves it like
any other program. A start whose `hermes-acp` cannot resolve fails with the
engine's launch-failure diagnostic naming the executable.

## Re-validation duty

Hermes merges near-daily; the pin above is a stale-the-moment-it-is-written
snapshot by design. Re-validate against your installed version before
relying on agent-specific behavior, and move the pin forward with fresh
evidence — [the re-validation duty](../agents.md#the-re-validation-duty)
states the full obligation, including the contract's honor-system duties
that no automated section can prove.
