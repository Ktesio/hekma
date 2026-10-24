---
title: Aider
description: 'Aider under Hekma — a manifest adapter with engine-observed metering via the OPENAI_API_BASE override; the shipped manifest is conformance-validated in CI.'
---

# Aider

Aider has no ACP mode, so it integrates through a manifest `adapter.toml` —
the one manifest-based entry in this gallery, [shipped and
CI-validated](manifests/aider/adapter.toml) by the conformance kit. Its
billing-grade metering rides the engine-observed channel: Aider honors an
OpenAI-compatible base-URL override, which is the wiring the manifest
declares.

## Register

Copy [`manifests/aider/adapter.toml`](manifests/aider/adapter.toml) into
a directory of your own (the example assumes you copied it to `./aider`),
then point `--manifest` at that directory and register:

```bash
hekma agent register my-aider --manifest ./aider
hekma agent start my-aider
```

The manifest pins no launch args — add the flags your Aider setup needs by
editing your copy. It declares no `{env:...}` substitutions anywhere
(manifest values stay literal; there is no substitution syntax to expand).

## Honesty table

The `pause` and `interaction` rows are the shipped manifest's own per-OS
Capability Declaration, proven by the conformance kit against a bundled
fake — not run-verified against the real binary. The metering rows are the
honest part: the override is a hypothesis from Aider's documented
interfaces, dated, never demonstrated here.

| Surface | Value | Evidence class |
|---------|-------|----------------|
| `pause` | `guaranteed` (linux), `guaranteed` (macos), `best-effort` (windows) | `contract-validated` |
| `interaction` | `guaranteed` (all OSes) | `contract-validated` |
| Metering source | `engine-observed` | `contract-validated` |
| Billing-grade tokens | `—` unless the operator wires the override: tokens flow only when `metering.upstream_base_url` names the real provider endpoint (the engine then meters the model traffic through its loopback proxy, injected as `OPENAI_API_BASE`). Without it: no billing-grade usage in the ledger — tiers attempted: observed: not-configured; sentinel: no-lines-seen (Aider prints token stats of its own but writes no `KTESIO_USAGE` sentinel) | `characterized` |
| Context usage | Aider's own on-screen token stats are display-only; the engine surfaces nothing extra for this entry | `characterized` |

Wiring the override, after registering:

```bash
hekma agent config set my-aider metering.upstream_base_url "https://api.openai.com"
```

> Verify the endpoint form against your provider before relying on it:
> OpenAI-compatible base URLs conventionally need the `/v1` suffix
> (`https://api.openai.com/v1`), and this wiring line is part of the
> `characterized` claim below — not run-verified here.

## Validation status

- **Upstream stability:** stable (the Aider project), but **this gallery's
  integration claims are `characterized`** — the base-URL override wiring is
  a hypothesis from Aider's documented `--openai-api-base`/`OPENAI_API_BASE`
  interfaces, dated 2026-10-23, NOT run-verified here.
- **What IS validated:** the shipped manifest itself. The conformance kit
  runs the full fixed section suite against it in CI — with its bare exec
  word shadowed on `PATH` by a bundled fake agent (CI never launches the
  real binary) — and two drift guards fail the build by name: a manifest
  that ships without this page, and a page that stops naming the
  manifest's exec word. "Conformant" means every machine-checkable duty of the
  [Adapter Contract](../adapter-contract.md#what-is-checked-versus-honored-the-tcks-honest-reach)
  held for the declaration; it is not a claim that real Aider honors the
  override.

## PATH requirement

The manifest's `exec` is the bare word `aider`, resolved through the
operator's `PATH` at start (the [manifest reference](../manifest.md)'s own
resolution rule). Hekma does not bundle, install, or pin the Aider binary —
you install it (per Aider's own docs), keep it on the `PATH` of the
environment `hekma` runs in, and `hekma agent start` resolves it like any
other program. A start whose `aider` cannot resolve fails with the engine's
launch-failure diagnostic naming the executable.

## Re-validation duty

The override wiring is the part re-validation must cover first: prove it
against your installed Aider version before trusting the metering row.
Every claim here is a stale-the-moment-it-is-written snapshot by design —
[the re-validation duty](../agents.md#the-re-validation-duty) states the
full obligation, including the contract's honor-system duties that no
automated section can prove.
