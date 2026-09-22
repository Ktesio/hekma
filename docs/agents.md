---
title: Supported Agents
description: The agents Hekma's adapters target, the exact versions each adapter was validated against, the PATH requirement, and the re-validation duty.
---

# Supported Agents

Hekma runs agents as adapter-backed processes: the built-in `acp` kind runs any Agent Client Protocol v1 agent — Hermes among them, via its native `hermes-acp` entry point — and is the recommended path for every ACP-capable agent; the built-in `hermes` native adapter also targets the Hermes gateway but is **deprecated** (see [below](#hermes---kind-hermes)); any other agent integrates through a manifest `adapter.toml` ([the manifest reference](manifest.md)). This page records **which agent versions the shipped adapters were validated against** — an adapter's honesty is only as good as its last validation, because agent upstreams move fast.

> **Isolation honesty.** Agent Home organizes and isolates an agent's files.
> It is not a security sandbox: supervised agents run as normal processes with
> your user's permissions.


| Agent | Adapter | Validation status | Validated against | Evidence |
|-------|---------|-------------------|-------------------|----------|
| Hermes (NousResearch) | native builtin (`--kind hermes`) — **deprecated; use `--kind acp`** | **Run-verified** — launched, supervised, and driven end-to-end (lifecycle, metering, budget, memory) under the recorded isolation sandbox | `v0.20.5` @ `41447a6d7063b2772b0c2f26a5b22d9bd444fb43` (2026-08-25) | primary-source verification plus end-to-end conformance passes (recorded in-repo) |
| opencode (anomalyco) | manifest adapter shape (no builtin) | **Paper-validated** — primary-source characterization + conformance mapping; never launched by this repo | `v1.18.27` @ `4b7e19e315cca414121ba1d61523fef74bb3ae8b` (2026-09-02 release) | primary-source characterization + conformance-mapping notes that fed the contract-v1 freeze |

## Hermes (`--kind hermes`)

> **Deprecation notice.** The `hermes` kind is **deprecated in favor of `--kind acp`** — Hermes Agent speaks the Agent Client Protocol natively via its `hermes-acp` entry point, and new registrations should use `--kind acp` (set the `acp.command`/`acp.args` config keys to that executable). The `hermes` kind keeps working unchanged; removal can happen only at a future **major** release, per the CLI-surface deprecation policy (see [the command reference](commands.md#hekma-agent-register-name---kind-kind---manifest-path)). The migration parity is shipped and tested: metering continues via the self-reported usage sentinel (under `acp` it rides the agent's stderr, since its stdout is the ACP protocol stream), and a `filesystem` Memory Backing still delivers the managed dir as `HERMES_HOME` under the `acp` kind.

The `hermes` builtin is compiled into the engine and declares a FIXED launch — `hermes gateway run --external-supervisor` — so Hekma supervises a foreground gateway process instead of the agent's own service manager. What the adapter declares:

- **Config mapping**: only the reserved `memory.dir` key → env `HERMES_HOME` (attaching a `filesystem` Memory Backing gives the gateway its per-instance home; with no backing attached the gateway receives NO `HERMES_HOME` and falls back to its own default home — see [the command reference](commands.md#hekma-agent-memory-attach-name---kind-kind)). The unified `model` key is a documented no-op for hermes.
- **Capabilities**: pause `best-effort` and interaction `guaranteed` on every OS; metering `self-reported`.
- **PATH requirement**: the launch's `exec` is the bare word `hermes`, resolved through the operator's `PATH` at start. Hekma does not bundle, install, or pin the Hermes binary — you install it (per Hermes' own docs), keep it on the `PATH` of the environment `hekma` runs in, and `hekma agent start` resolves it like any other program. A start whose `hermes` cannot resolve fails with the engine's launch-failure diagnostic naming the executable.

**Validation pin**: the adapter's behavior was verified against Hermes at `v0.20.5`, commit `41447a6d7063b2772b0c2f26a5b22d9bd444fb43` (verified 2026-08-25). CI never launches the real gateway: the conformance passes run under the recorded `hermes_shim` PATH-sim sandbox (an isolated stand-in that re-execs a test helper), so the suite is deterministic and network-free — the real-binary validation is the pinned manual pass recorded above it.

## ACP agents (`--kind acp`)

The `acp` builtin is agent-agnostic: any executable speaking Agent Client Protocol v1 over stdio is a Hekma backend with no per-agent adapter work. Agents known to work or expected to:

- **Hermes Agent** — natively, via its dedicated `hermes-acp` entry point (point `acp.command` at `hermes-acp`). This is the migration path from the deprecated `hermes` kind (notice above); the parity proofs — metering via the stderr sentinel and `HERMES_HOME` delivery under a `filesystem` Memory Backing — are shipped and tested.
- **Gemini CLI** — speaks ACP when started with its ACP-mode flag: `acp.command` = `gemini`, `acp.args` = `--experimental-acp` (the long-documented IDE-integration spelling; newer builds accept the shortened `--acp`).
- **Claude Code** — has no native ACP mode; it integrates through an ACP adapter binary that wraps the Claude Code SDK and speaks the protocol on its behalf (point `acp.command` at that adapter, with whatever args it documents). Hekma ships no such adapter — treat any third-party one as its own supply-chain decision.

**Metering honesty, per the tiered design.** An ACP agent's `usage_update` notifications carry its session **context** window — surfaced under the additive `acp_context_usage` field, never billed. Billing-grade tokens come only from the metering tiers: the **self-reported sentinel** (under `acp` the cooperative agent writes `KTESIO_USAGE {json}` lines to its **stderr**, its stdout being the protocol stream) or the **engine-observed** loopback channel when the agent honors a base-URL override (`metering.upstream_base_url`). With neither, the token/cost cells show the honest `—` and a notice naming the tiers attempted — never a fabricated figure. All three counts (input/output/cached) meter identically under `acp` as under any other kind since the cached-token pipeline.

**Validation status: contract-validated, not agent-pinned.** The transport is validated against a scripted contract-twin agent (`fake_acp_agent` in `hekma-conformance`), which drives every protocol behavior the epic pins — handshake, chunked updates, `usage_update`, tool-call permission denial, session resume, version counter, malformed lines. The real agents are exercised by skip-unless-present smoke tests (`hermes-acp`, `gemini` — run them on a machine where the binary exists; they are honest no-ops in CI's network-free sandbox), so no upstream version pin is claimed here yet: re-validate against your installed agent version before relying on agent-specific behavior, per the duty below.

## opencode (paper-validated)

opencode has no builtin adapter: it integrates as a manifest adapter whose `[lifecycle.start]` points at its `serve` command, declaring `interaction: http` (the additive documentary channel) with `XDG_DATA_HOME` + `XDG_CONFIG_HOME` as its isolation levers. Its contract behavior was **validated on paper only** — a primary-source characterization of the `v1.18.27` sources plus a conformance mapping that shaped the Adapter Contract v1 freeze. Nothing in this repository has launched opencode; treat its adapter shape as a starting point and re-validate before relying on it.

## The re-validation duty

Both upstreams move fast (opencode ships multiple releases per week; Hermes merges near-daily). Every version pin on this page is a **stale-the-moment-it-is-written** snapshot by design: the pins make the validation auditable, not permanent. Whoever touches an adapter — or ships a release that leans on one — re-validates against the pinned release before trusting agent-specific behavior, and moves the pin forward with fresh evidence. The contract's honor-system duties (the `{env:VAR}` render guarantee, self-update pinning, config-layer disclosure — see [the Adapter Contract](adapter-contract.md#what-is-checked-versus-honored-the-tcks-honest-reach)) are exactly the parts re-validation must cover, because no automated section can.

## Troubleshooting: hermes won't launch

Launch failures (`hermes: command not found`, immediate `failed`, or behavior drift after a Hermes upgrade) are diagnosed in [the canonical troubleshooting section](troubleshooting.md#hermes-wont-launch-command-not-found--immediate-failure) — kept in one place so the two pages cannot drift. This page owns the validation pins and the re-validation duty; that page owns the failure surfaces.
