---
title: GitHub Copilot CLI
description: 'GitHub Copilot CLI under Hekma — ACP via its --acp --stdio server mode (public preview); characterized from upstream docs, not run-verified here.'
---

# GitHub Copilot CLI

GitHub's Copilot CLI ships an ACP server mode, so it runs under Hekma's
built-in `--kind acp` adapter with no per-agent adapter work. The mode is in
public preview upstream; this entry is characterized from upstream
documentation and has not been run-verified here.

## Register

```bash
hekma agent config set my-copilot acp.command "copilot"
hekma agent config set my-copilot acp.args "--acp --stdio"
hekma agent register my-copilot --kind acp
hekma agent start my-copilot
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
| Billing-grade tokens | `—` — no billing-grade usage in the ledger — tiers attempted: observed: not-configured; sentinel: no-lines-seen; no context usage reported (ACP usage_update is context-grain, never billed). No sentinel emission and no base-URL override is documented upstream | `characterized` |
| Context usage | an agent-reported `usage_update` would surface under `acp_context_usage` — context-grain, never billed; whether this agent reports one is unverified here | `characterized` |

## Validation status

- **Upstream stability:** preview (the ACP server mode is in public preview
  upstream since 2026-01-28).
- **Evidence class:** `characterized` — upstream documentation, dated
  2026-10-23, NOT run-verified here.
- **Validated against:** nothing pinned.

## PATH requirement

The launch's `exec` is the bare word `copilot`, resolved through the
operator's `PATH` at start. Hekma does not bundle, install, or pin the
Copilot CLI binary — you install it (per GitHub's own docs), keep it on the
`PATH` of the environment `hekma` runs in, and `hekma agent start` resolves
it like any other program. A start whose `copilot` cannot resolve fails with
the engine's launch-failure diagnostic naming the executable.

## Re-validation duty

The ACP server mode is a preview surface and can change or disappear in any
upstream release; every characterization here is a
stale-the-moment-it-is-written snapshot by design. Re-validate against your
installed version before relying on agent-specific behavior — [the
re-validation duty](../agents.md#the-re-validation-duty) states the full
obligation, including the contract's honor-system duties that no automated
section can prove.
