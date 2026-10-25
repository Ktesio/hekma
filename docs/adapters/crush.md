---
title: Crush
description: 'Crush under Hekma — through an unofficial third-party crush-acp adapter binary; characterized, not run-verified here.'
---

# Crush

Charm's Crush has no native ACP mode (upstream ACP support is still an open
issue with draft PRs, as of 2026-10-23); it integrates through an
unofficial third-party `crush-acp` adapter binary that speaks the protocol
on its behalf. Hekma ships no such adapter — treat the adapter as its own
supply-chain decision, with the unofficial status on top.

## Register

The adapter binary below is the unofficial third-party wrapper, not Crush
itself and not anything Hekma distributes:

```bash
hekma agent config set my-crush acp.command "crush-acp"
hekma agent register my-crush --kind acp
hekma agent start my-crush
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
| Billing-grade tokens | `—` — no billing-grade usage in the ledger — tiers attempted: observed: not-configured; sentinel: no-lines-seen; no context usage reported (ACP usage_update is context-grain, never billed). No sentinel emission is documented upstream; the engine-observed tier is plausible (Crush supports custom providers) — a hypothesis, unproven here | `characterized` |
| Context usage | an agent-reported `usage_update` would surface under `acp_context_usage` — context-grain, never billed; whether the adapter reports one is unverified here | `characterized` |

## Validation status

- **Upstream stability:** third-party adapter — unofficial (Crush's own ACP
  support is still an open upstream issue, and the adapter is an
  independently maintained wrapper outside this repository's).
- **Evidence class:** `characterized` — upstream documentation, dated
  2026-10-23, NOT run-verified here.
- **Validated against:** nothing pinned.

## PATH requirement

The launch's `exec` is the bare word `crush-acp`, resolved through the
operator's `PATH` at start. Hekma does not bundle, install, or pin the
adapter (or Crush) — you install whichever adapter you choose, vet it
yourself, keep it on the `PATH` of the environment `hekma` runs in, and
`hekma agent start` resolves it like any other program. A start whose
`crush-acp` cannot resolve fails with the engine's launch-failure diagnostic
naming the executable.

## Re-validation duty

An unofficial adapter over an upstream without native ACP support is the
least stable integration in this gallery; every characterization here is a
stale-the-moment-it-is-written snapshot by design. Re-validate your adapter
+ Crush combination before relying on agent-specific behavior — [the
re-validation duty](../agents.md#the-re-validation-duty) states the full
obligation, including the contract's honor-system duties that no automated
section can prove.
