---
title: Adapter Contract
description: The versioned contract between the Hekma engine and every agent adapter — trait surface, capability declarations, version negotiation, and the versioning and deprecation policy.
---

# Adapter Contract (v1)

The **Adapter Contract** is the agreement between the Hekma engine and every agent adapter — native builtins and manifest adapters alike. An *agent* here is the third-party program an adapter makes runnable — a personal agent such as Hermes Agent or a coding agent such as opencode — never something Hekma authors. It lives in the `hekma-adapter-api` crate (the schema's single home), is versioned independently of the engine under its own semver, and is **frozen at v1 (`1.0.0`)** as of 2026-09-04 after validation against a second agent (opencode) and its conformance passes. This page is the contract's normative home: what an adapter implements, what the engine enforces, and the rules for changing any of it.

Everything here applies uniformly to both adapter kinds ("two kinds, one trait"): a native builtin declares the same shapes in code that a manifest adapter declares in `adapter.toml`. **One deliberate exception: version negotiation.** `contract_version` and the negotiation gate below are **manifest-only** — only a manifest adapter carries a `contract_version` and can fail negotiation; native builtins declare no contract version in code and are always compatible with the engine that ships them (a builtin and its engine release from the same build). The concrete manifest syntax is documented in [the manifest reference](manifest.md); the Conformance Test Kit ([testing](testing.md#the-conformance-test-kit)) proves an adapter honors the sections it can exercise.

## The adapter trait surface

An adapter implements (or a manifest declares) exactly:

| Surface | Meaning |
|---------|---------|
| `kind` | The adapter's identifier (e.g. `mock`, `hermes`, or the manifest's `[adapter].kind`), stored on the Agent Instance |
| `capabilities()` | The per-OS Capability Declaration (below) |
| `metering_source()` | The Metering Source: `self-reported` or `engine-observed` (a "no viable source" adapter is a validation error, never a variant) |
| `config_mapping()` | The unified→native config mapping; the default is an EMPTY mapping (an unmapped documented key is delivered nowhere — a no-op, never a guess) |
| `start` / `stop` / `pause` / `resume` | The lifecycle templates. Only `start` is mandatory; an omitted `stop` means the engine's process termination IS the normative stop (below) |
| `interaction` (manifest-only section) | The declared interaction channel: `stdio` or `http` (documentary — see below) |

The engine is the sole launcher: every adapter is spawned through the identical process-backend mechanism, and the engine pipes the child's stdin **iff the adapter's current-OS `interaction` capability is declared `guaranteed` or `best-effort`** — an adapter that declares `unsupported` interaction spawns with **no stdin pipe** (its write end would never deliver input nor close, and a child that blocks reading a never-written pipe at startup would hang silently). This is a hang-safety stance, not a capability bypass: `send_input` still fails fast and typed for an unsupported declaration. Adapter trait lifecycle methods are the declaration surface; the engine's supervisor owns the actual transitions.

## Capability declarations

Capabilities are declared **per OS** (`linux`, `macos`, `windows`) with one of three support levels:

- `guaranteed` — the engine may rely on it.
- `best-effort` — surfaced honestly (a qualifier cause on the transition, e.g. `pause-best-effort`), never silently treated as guaranteed.
- `unsupported` — the command fails fast, quoting the declaration; nothing is faked.

A capability with no entry for an OS projects to `unsupported`. A declaration must have at least one non-`unsupported` entry somewhere — an adapter that supports nothing is not viable and is rejected at registration. Modeled capabilities in v1: `pause` and `interaction`.

Per-OS honesty is the adapter author's job: the contract lets an author declare any level on any OS and holds them to it; it does not police vendor OS-support stances (see the isolation-lever decision in the checkpoint record below).

## Lifecycle and stop semantics

- **Start** is the only mandatory template. The resolved launch (exec/args/env) is snapshotted at registration.
- **An omitted stop template is normative**: the engine's process termination (signal, then escalation) IS the stop; the TCK asserts the child process exits. Adapters to agents with a graceful-stop verb declare a stop template. There is no graceful-stop acknowledgment concept in v1: an agent's self-exit (e.g. an in-chat restart hand-off) is an unrequested exit handled by the crash/restart policy — any non-zero exit while Running is a crash, and the restart policy relaunches with the same persisted launch.
- **Readiness vs liveness**: v1 defines no readiness concept distinct from process liveness and no endpoint-discovery surface. A supervisor that must know where an HTTP-native child listens should pin its port via the start template and own collision handling, or parse the child's documented startup line — that is an adapter-convention concern, not contract machinery.
- **Agent-side abort** is out of contract scope: verbs are start/stop/pause/resume. An agent's own abort facility (e.g. ending one session's work inside a still-running server) is the agent's business; the contract's stop asserts process-level outcomes only.

## Interaction channels

`[interaction].channel` is a closed enum: `stdio` (the spawned child's OS stdin pipe — the only channel the engine delivers through in v1) or `http` (an HTTP-native interaction surface, added at the v1 freeze as the ratified channel-vocabulary option).

`http` is **additive vocabulary, deliberately documentary**: the engine never branches on the declared channel — the stdin pipe follows the capability declaration (delivered iff the current-OS `interaction` level is `guaranteed`/`best-effort`, per the launcher rule above, never on the channel value), and `send_input` still writes the child's stdin (failing fast and typed where the declaration says unsupported). Its purpose is honesty and registerability: an HTTP-native agent (e.g. opencode, whose programmatic surface is `POST /session/:id/message` + SSE and whose spawned `serve` never reads stdin) declares `http` to name its real transport and registers with `interaction` supported instead of being forced into an all-unsupported (and therefore illegal) declaration. A real engine-side HTTP send implementation is a **post-v1** change under the versioning policy; the relaxation alternative (letting a single-capability adapter declare `unsupported` everywhere) was **declined** — the viability bar stays.

**The interaction section is channel-blind (stated plainly):** the engine and the Conformance Test Kit never branch on the declared channel value in v1. An adapter that declares `http` is exercised through the same stdin probe as a `stdio` adapter — so for an HTTP-native agent, an `interaction: pass` proves the *declaration-consistent fail-fast and the engine's stdin seam*, **not** that the agent's real HTTP surface works. Proving an HTTP transport is the adapter author's own validation duty (the same per-release re-validation duty the isolation-keys rule imposes); a channel-aware TCK section needs engine-side HTTP send machinery and is a post-v1 change.

## Metering

- `self-reported`: the agent's usage accounting reaches the engine as `KTESIO_USAGE {json}` sentinel lines on the captured stdout. **The channel is adapter-implemented** (contract v1): the stream may be native or synthesized by an adapter shim over the agent's own surfaces (SSE tails, session reads); a shim derives `sequence` from the agent's per-Run message ordinals so the `UNIQUE(instance, run, sequence)` replay-dedup invariant holds identically. Delayed batches reconcile without double-counting in both variants.
- `engine-observed`: the engine meters the agent's model traffic through a per-instance loopback forward proxy — no agent cooperation needed.

**Unknown-vs-zero is a contract stance**: when the provider omits usage, an adapter must surface it as unknown — never coerce it to zero. A zero event asserts the provider reported zero; an absent event means the usage is unknown. (Hermes labels cost honesty; opencode silently coerces missing usage to zero — the contract sides with the label.)

## Memory

Memory Backing vocabulary is part of the frozen v1 surface, carried on `hekma agent memory attach|detach --json` (frozen in the v1 wire set; before it, the surface was human-output-only):

- **Kinds** (snake_case wire strings, verbatim): `filesystem` (an engine-managed directory inside the Agent Home, byte-durable across restarts) and `native` (an explicit delegation marker).
- **Guarantee levels** (snake_case wire strings, verbatim): `managed_dir_byte_durable` and `home_persistence_only`.
- The memory commands' outputs are reachable via `--json` (attach and detach emit versioned documents); the documents are frozen key-set-for-key-set by the compatibility tests, and the freeze landed as the ONE announced key-set edit (announced in the release notes). There is deliberately NO memory read verb in v1 — adding one (or touching these key-sets again) is a post-v1 change under the policy below, explicitly out of scope at the freeze.

Adapters that cannot consume a memory directory are fine: the reserved `memory.dir` key is delivered only through the adapter's own declared mapping (delivery is offered, never imposed), and an unmapped key is delivered nowhere — never silently dropped into a file the agent will not read.

## Configuration delivery

The `[config]` mapping turns documented unified keys into native mechanisms (`env`, `flag`, `file`). v1's normative stances from the second-agent validation:

- **JSON-native config agents**: the documented pattern is **env-content delivery** — the agent's config-content environment variable carries the runtime config (e.g. opencode's `OPENCODE_CONFIG_CONTENT`). A format-qualified `file` target (JSON alongside TOML) is **reserved post-v1**; the current `file` target renders TOML only.
- **`{env:VAR}` substitution honesty**: agents that substitute `{env:VAR}` placeholders and render an unset variable as a silent empty string impose a duty on the adapter — every substituted variable must be guaranteed set, or the render must fail with a named reason, before the child launches. This is the freeze's only behavioral rider; a silently-empty rendered config is a contract violation.
- **Self-updating agents** must have their update-disable mechanism mapped and delivered by the start seam, and adapter docs must state whether the agent's config chain contains a higher-authority (managed/MDM) layer that could override supervisor-delivered values (so an operator can verify the pin survived).
- **Isolation keys**: adapters document the environment levers that give each instance its own data/config roots (Hermes: `HERMES_HOME`; XDG-based agents: `XDG_DATA_HOME` + `XDG_CONFIG_HOME`). **Per-instance isolated roots inside each Agent Home are the recommended normative posture** — they make shared-root concurrency out of contract scope. Where a lever is an undocumented agent interface, the adapter must say so and re-validate per pinned release (undocumented levers are acceptable with that per-release obligation, not silently).

### What is checked versus honored (the TCK's honest reach)

The contract's duties split into two classes, and adapter authors should know which is which:

- **Declaration-checked** — the Conformance Test Kit exercises these mechanically: the Capability Declaration's persisted projection, lifecycle transitions and stop semantics, pause per the declared level, declared config-mapping delivery (through a dump seam), metering per the declared source, declared Memory Backing delivery, and interaction fail-fast. A `pass` in these sections is machine-checked evidence.
- **Honor-system** — v1 ratifies these duties but NO section can exercise them, because proving them requires the real agent's behavior, not a declaration: the `{env:VAR}` render guarantee (every substituted variable set or the render fails with a named reason), self-update pinning (the update-disable mapping actually survives the agent's own update chain), and the config-layer disclosure (the adapter documents higher-authority config layers). These are adapter-author obligations enforced by review and per-release re-validation, **not** by a TCK `pass` — an all-green report is not a claim that an adapter honors them.

The report never asserts more than its sections exercised; "conformant" means every machine-checkable duty held, not that the honor-system duties were proven.

## Versioning

The contract carries a semantic version; **the engine states which contract versions it accepts and rejects mismatches informatively** (FR-30):

- **The rule: a manifest is compatible iff its `contract_version` major matches the engine's contract major.** A different major always fails registration, naming BOTH versions and quoting the rule. Read the rule precisely: major-match is **necessary but not sufficient**. It guarantees only that an engine will never reject a manifest SOLELY for version distance within the major — an older engine still enforces its own frozen schema, so a NEWER same-major manifest using a post-`1.0.0` section fails that engine's unknown-key check. Same-major forward compatibility comes from the schema staying additive within a major, not from the major check alone:

  ```text
  incompatible adapter contract: manifest declares 2.1.0, engine speaks 1.0.0 — compatible iff the major versions match (contract v1 policy, docs/adapter-contract.md#versioning)
  ```

- **When the gate runs** (manifest-kind scope, stated for operators): negotiation fires at **registration** and again whenever a start **re-reads the manifest from disk** — the legacy/launch-less start fallback. An instance with a persisted launch snapshot restarts from that snapshot without re-reading the manifest, so a post-registration edit of `adapter.toml` to a foreign major is fatal for a fallback-start instance and invisible to a snapshot-start one. That asymmetry is deliberate (a persisted launch is the engine's recorded truth; the fallback re-read is a fresh load), but it is an asymmetry: if you edit a manifest, re-register (or remove + register) rather than relying on the edit taking effect.
- **Strict `X.Y.Z` parsing** (resolved at the freeze): the manifest's `contract_version` must be a strict semver triple — no `v` prefix, no partial versions (`1`, `1.0`). Prerelease and build-metadata suffixes (`1.0.0-rc.1+build.5`) parse as semver; **negotiation compares majors only**, so a same-major prerelease manifest is compatible. Prerelease spellings are development-time conveniences, not a published compatibility promise — release tooling always publishes clean triples.
- **Pre-v1 (`0.x`) manifests are not grandfathered**: the contract was never published under 0.x, so the seed values (0.1.0–0.4.0) carry no back-compat obligation. They fail registration exactly like any other major mismatch.
- **Within a major**, changes are additive (a new optional manifest section, a new enum variant, a new optional JSON field). Anything that removes or renumbers is breaking and requires the next major.
- The Rust API of `hekma-adapter-api` is guarded by the CI `semver` job (`cargo-semver-checks`) on two baselines: an **armed in-repo baseline** that diffs the frozen surface against the contract-v1 freeze commit (`4119db3`) on every run, and the crates.io-published baseline once the crates publish to crates.io. The serialized wire shapes and exit codes are guarded by the workspace compatibility tests (`crates/hekma/tests/agent_cli.rs`), which fail CI on an unannounced change on all three OSes.

## Deprecation policy

*Ratified by Islam, 2026-09-04 — quoted verbatim, the whole ratified policy:* **within a major, deprecations announced ≥1 minor ahead via CHANGELOG/RELEASE_NOTES + doc notices; removals only at next major; enforced by semver-checks CI.** The semver-checks enforcement for the Rust surface is **armed now** against the in-repo freeze-commit baseline (see the gate note below); the crates.io-published-baseline comparison arms at the crates' first crates.io publish. Until then the announcement rule for non-Rust surfaces is enforced by the release-notes discipline itself. The ratified sentence names the Adapter Contract and its docs; it deliberately does not expand to other surfaces — each other surface's own announcement machinery (e.g. the `--json` key-set freeze rule) governs it in the meantime.

## The semver gate, stated plainly

The CI `semver` job runs `cargo-semver-checks` on **two baselines**:

1. **In-repo baseline (armed):** every run diffs `hekma-adapter-api`'s public Rust surface against the contract-v1 freeze commit (`4119db3`) via `--baseline-rev`. Any breaking public-surface change vs the baseline — removal, rename, or signature — **fails CI red**, announced or not (announcement itself is review discipline; the gate cannot read intent). This closes the freeze's original guard gap, where no verification failed on a public-surface break during the pre-publish window.
2. **crates.io baseline (dormant until first publish):** the published-baseline comparison cannot fire until the crates publish to crates.io; today that leg checks crates.io, sees the 404, and emits a notice saying exactly that. It arms itself automatically at first publish, and its cache plumbing is already version-keyed and armed-ready.

What additionally protects the contract are the workspace compatibility tests and the review discipline.

## Ratified freeze decisions (recorded)

The pre-freeze paper validation of opencode raised six change proposals and eleven freeze risks. Every one carries an explicit ratified verdict (ratified by Islam, 2026-09-04); none silently dropped. The in-repo planning record holds the one-line rationale for each; the normative text above is where the ratified decisions live.

| Topic | Verdict |
|----|---------|
| Interaction channel vocabulary | Option (i) ratified: `InteractionChannelKind::Http` added as v1 vocabulary, engine never branches on channel. The alternative — relaxing `has_any_support` — DECLINED. HTTP send implementation deferred post-v1. |
| Stop semantics | Ratified (documentary): an omitted `[lifecycle.stop]` means engine signal-termination is the normative stop; TCK asserts child exit. No graceful-acknowledgment concept in v1. |
| Self-reported metering | Ratified (documentary): `self-reported` covers adapter-implemented shims; `sequence` derives from the agent's per-Run message ordinals; both sources carry the no-double-count guarantee. |
| JSON-config delivery | Ratified (documentary): env-content delivery is the documented pattern for JSON-config agents; format-qualified `file` target reserved post-v1. PLUS the one behavioral rider: `{env:VAR}` substitution must guarantee set variables or fail the render with a named reason. |
| Self-updating agents | Ratified (documentary): self-updating agents must have their update pin mapped and delivered; adapter docs must disclose higher-authority config layers. Optional TCK assertion deferred. |
| Isolation keys | Ratified (docs-only): isolation keys named per agent (opencode: `XDG_DATA_HOME` + `XDG_CONFIG_HOME`) with the churn caveat — undocumented levers acceptable with per-release re-validation. |
| Memory wire freeze | Ratified: the memory wire froze in v1 — typed snake_case `GuaranteeLevel`/`MemoryBackingKind` strings adopted verbatim on the new `--json` documents (the ONE announced key-set edit); adapters that cannot consume `memory.dir` simply declare no mapping (delivered nowhere, honestly surfaced). |
| Unknown-vs-zero | Ratified: omitted usage is surfaced as unknown, never coerced to zero (surfaced-not-silent labels). |
| Readiness vs liveness | Deferred-post-v1: no readiness/liveness distinction or endpoint-discovery surface in v1; pin-the-port or startup-line-parsing are adapter conventions. |
| Isolated-roots posture | Recommended normative (docs-only): per-instance isolated roots for networked adapters; shared-root modes are out of contract scope. |
| Agent-side abort | Deferred-post-v1: agent-side abort is out of contract; engine-owned termination semantics only, TCK asserts process-level outcomes. |
| Per-OS honesty | Docs-only: per-OS honesty is already the declaration's job — a windows `best_effort` cap for WSL-recommended agents is an adapter-author choice, not a contract mandate. |

## What changed at the freeze

For adapter authors upgrading from a pre-freeze seed manifest:

1. Set `contract_version = "1.0.0"`. Any `0.x` value now fails registration with the both-versions error quoted above.
2. `[interaction].channel` accepts `"http"` in addition to `"stdio"` — optional, documentary.
3. The manifest syntax is otherwise unchanged; every 0.x-era section parses identically under 1.0.0.

## See Also

- [Adapter manifest (`adapter.toml`)](manifest.md) — the concrete manifest syntax.
- [Command reference](commands.md) — the `hekma agent` commands, including the memory `--json` documents.
- [Architecture](architecture.md) — how the engine, the contract crate, and the conformance kit fit together.
- [The Conformance Test Kit](testing.md#the-conformance-test-kit) — prove your adapter conforms.
