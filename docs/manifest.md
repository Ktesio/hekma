---
title: Adapter Manifest
description: The adapter.toml shape that registers an agent — how to launch it, its per-OS capabilities, its metering source, and its config mapping.
---

# Adapter Manifest (`adapter.toml`)

A **manifest adapter** registers any agent from a single `adapter.toml`. It declares how the engine launches the agent process, the agent's per-OS capabilities, its metering source, and (optionally) how unified config keys map into the agent's native mechanism. Register one with:

```bash
hekma agent register <name> --manifest <dir-or-adapter.toml>
```

The manifest is parsed and validated **before** any state is written. Unknown keys are rejected (typo protection), and the first missing or invalid mandatory section is named in the error.

## Complete Example

```toml
contract_version = "1.0.0"

[adapter]
kind = "my-agent"
name = "My Agent"

[lifecycle.start]
exec = "my-agent"
args = ["--serve"]
env = { LOG_LEVEL = "info" }

[lifecycle.stop]
exec = "my-agent"
args = ["--shutdown"]

[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "best-effort"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"

[interaction]
channel = "stdio"

[config.model]
env = "MODEL"
```

## Mandatory Sections

A manifest is valid only if it declares all of the following. Validation reports the first one that is missing, empty, or invalid.

### `contract_version`

The Adapter Contract version the manifest targets, as a semver string (current: `"1.0.0"`). A non-semver value is rejected. The parse is **strict `X.Y.Z`**: no `v` prefix and no partial versions (`1`, `1.0`); a prerelease or build-metadata suffix (`1.0.0-rc.1+build.5`) parses, and negotiation compares majors only.

The engine **negotiates** at registration (contract v1, FR-30): a manifest is rejected when its `contract_version` **major** differs from the engine's — *compatible iff the major versions match*. Read the rule precisely: major-match is **necessary but not sufficient**. It guarantees only that the engine never rejects a manifest solely for version distance within the major — a NEWER same-major manifest that uses a section this engine's schema does not know yet still fails the unknown-key check (`deny_unknown_fields`), because the schema stays additive only within a major. A mismatch fails the load naming **both** versions and quoting the rule, e.g.:

```text
incompatible adapter contract: manifest declares 2.1.0, engine speaks 1.0.0 — compatible iff the major versions match (contract v1 policy, docs/adapter-contract.md#versioning)
```

Pre-v1 `0.x` versions are **not** grandfathered: the contract was never published under 0.x, so those seed values carry no back-compat obligation. The full versioning and deprecation policy lives in the [Adapter Contract](adapter-contract.md#versioning).

### `[adapter]`

Adapter identity.

| Field | Type | Required | Meaning |
|-------|------|----------|---------|
| `kind` | string | yes | The adapter kind, stored on the Agent Instance |
| `name` | string | no | A human-friendly name |

### `[lifecycle]`

Lifecycle op templates. At minimum `[lifecycle.start]` is required; `stop`, `pause`, and `resume` are optional.

Each op template has:

| Field | Type | Required | Meaning |
|-------|------|----------|---------|
| `exec` | string | yes | The executable to run (a program name on `PATH`, or an absolute path) |
| `args` | array of strings | no | Positional arguments (default empty) |
| `env` | table of strings | no | Environment overrides (default empty) |

The resolved start launch (`exec`/`args`/`env`) is snapshotted **at registration**: editing the manifest afterward has no effect on an already-registered instance until it is removed and re-registered.

**An omitted `[lifecycle.stop]` is normative** (contract v1): it means the engine's process termination (signal, then escalation) IS the stop — the child process exits, and that exit is what the Conformance Test Kit asserts. Adapters whose agents own a graceful-stop verb declare a stop template; agents without one (session-shaped agents, servers) legitimately omit it. There is no graceful-stop acknowledgment concept in v1: an agent that exits on its own (e.g. an in-chat restart hand-off) is simply an unrequested exit handled by the crash/restart policy.

### `[capabilities]`

A non-empty, per-OS Capability Declaration. Each capability is a sub-table keyed by OS (`linux`, `macos`, `windows`) whose value is a support level: `"guaranteed"`, `"best-effort"`, or `"unsupported"`. A capability with no entry for an OS projects to `unsupported`. The declaration must have at least one non-`unsupported` entry.

Modeled capabilities today are `pause` and `interaction`:

```toml
[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "best-effort"
```

`pause` is typically `guaranteed` on Unix (SIGSTOP) and `best-effort` on Windows.

### `[metering]`

A viable Metering Source. `source` is one of:

- `"self-reported"` — the agent emits its own usage accounting (`KTESIO_USAGE {json}` sentinel lines on stdout).
- `"engine-observed"` — the engine meters the agent's model traffic through a per-instance loopback proxy. The operator points the agent's OpenAI-compatible `base_url` at the engine (via the config mapping below) and sets `metering.upstream_base_url` to the real provider endpoint.

```toml
[metering]
source = "self-reported"
```

A `self-reported` agent forwards its own usage accounting by emitting `KTESIO_USAGE {json}` sentinel lines on stdout. The JSON payload carries three agent-supplied fields (snake_case):

```json
{"sequence": 0, "input_tokens": 128, "output_tokens": 512}
```

| Field | Type | Meaning |
|-------|------|---------|
| `sequence` | non-negative integer | A per-Run monotonic ordinal stamped by the agent. It is the replay-dedup key: a re-delivered batch with the same `sequence` is recognized and not double-counted. |
| `input_tokens` | non-negative integer | Input token count for the event. |
| `output_tokens` | non-negative integer | Output token count for the event. |

The engine stamps the Run id, the instance, the Metering Source, and the timestamp. A malformed usage line is a diagnostic (ignored), never fatal.

**The `self-reported` channel is adapter-implemented** (contract v1): the sentinel stream may be produced natively by the agent OR synthesized by an adapter shim over the agent's own usage surfaces (API tails, session reads). A shim derives `sequence` from the agent's per-Run message ordinals, so the replay-dedup invariant holds identically. When an agent's provider omits usage, the adapter must surface it as **unknown** — never coerce it to zero (a `0` token event asserts the provider reported zero; an omitted event means the usage is unknown). The normative v1 path for agents that report nothing themselves is `engine-observed`; for agents that expose their usage through any reachable surface, a `self-reported` shim is equally conformant — both guarantee that delayed batches reconcile without double-counting.

## Optional Sections

### `[interaction]`

Interaction channel wiring. Optional: omitting this section entirely still means `"stdio"` — the declared channel is documentary (the engine never branches on it); the stdin pipe follows the adapter's `interaction` **capability declaration** instead, delivered iff the current-OS level is `guaranteed`/`best-effort` (see [the Adapter Contract](adapter-contract.md#interaction-channels)).

| Field | Type | Meaning |
|-------|------|---------|
| `channel` | closed enum | The interaction channel: `"stdio"` (the spawned child's OS stdin pipe) or `"http"` (an HTTP-native interaction surface, e.g. an agent's loopback server). An unrecognized value is rejected at parse time. |

`"http"` is documentary vocabulary (contract v1, CP-6.5-a option (i)): it names where the adapter's agent really takes interaction, so an HTTP-native agent declares `interaction` supported honestly. v1 ships no engine-side HTTP delivery — the engine does not branch on the declared channel, and `hekma agent send` still writes the child's stdin; an adapter whose agent cannot read stdin should declare `interaction` unsupported on OSes where that is true (an adapter whose ONLY capability key is declared unsupported everywhere is rejected as non-viable). A real HTTP send implementation is a post-v1 change under the [versioning policy](adapter-contract.md#versioning).

### `[config]` — unified → native config mapping

Maps documented unified config keys into the agent's native mechanism at start time, so operators configure any agent in one vocabulary. Each `[config.<key>]` sub-table declares **exactly one** of three targets:

```toml
# Deliver as an environment variable.
[config.model]
env = "MODEL"

# Deliver as a CLI flag (rendered as two argv tokens: --model <value>).
[config.model]
flag = "--model"

# Render into a native TOML file inside the Agent Home, at a dotted native key.
[config.model]
file = { path = "config/agent.toml", key = "llm.model" }
```

Notes:

- A `file` target's `path` is **relative to the Agent Home**; an absolute path or one escaping the home is rejected at load time (the engine is the sole writer).
- A documented key the adapter maps nowhere is a silent no-op — not every adapter supports every unified key.
- `agent.*` pass-through keys are delivered verbatim (by convention, as an env var named by the key tail) without a mapping entry.
- For a `secret:NAME` value, the resolved cleartext is delivered into the native target while every Hekma display of the same key stays masked. Prefer `env`/`file` targets over `flag` for secret-carrying keys — an argv flag is visible to other local users on the process list.
- **Agents with JSON-native config**: v1's documented pattern is env-content delivery — map a unified key to the agent's config-content environment variable (e.g. `OPENCODE_CONFIG_CONTENT`) and the start seam delivers the value with no file format or placement question. A format-qualified `file` target (JSON alongside TOML) is reserved post-v1; the current `file` target renders a TOML document only.
- **`{env:VAR}` substitution honesty** (contract v1 rider): some agents substitute `{env:VAR}` placeholders in their own config and render an **unset variable as an empty string, silently**. An adapter to such an agent MUST map every substituted key through a delivery that guarantees the variable is set — or fail the render with a named reason — before the child launches. A silently-empty rendered config is a contract violation, not a quirk to tolerate.
- **Self-updating agents** (contract v1): an adapter for an agent that downloads its own updates MUST map that agent's update-disable mechanism (env or config) so the pinned, supervised binary stays pinned. The adapter docs must additionally state whether the agent's config chain contains a higher-authority (managed/MDM) layer that can override supervisor-delivered values, so an operator on a managed machine can verify the pin survived.
- **Isolation keys**: an adapter documents which environment levers give each Agent Instance its own data/config roots (for Hermes, `HERMES_HOME`; for XDG-based agents like opencode, `XDG_DATA_HOME` + `XDG_CONFIG_HOME`). Per-instance isolated roots inside each Agent Home are the RECOMMENDED normative posture (they make shared-root concurrency out of scope); where a lever is an undocumented agent interface, say so and re-validate it per pinned release. Per-OS support levels remain the declaration's own honesty job — an agent whose vendor recommends a compatibility layer on Windows is best declared `best-effort` there, but that cap is the adapter author's choice, not a contract mandate.

## See Also

- [Adapter Contract](adapter-contract.md) — the versioned contract this manifest speaks: negotiation, versioning/deprecation policy, and the ratified v1 decisions.
- [Command reference](commands.md) — the `hekma agent` commands and unified config keys.
- [Architecture](architecture.md) — the Adapter Contract, the Usage Ledger, and budget enforcement.
- [The Conformance Test Kit](testing.md#the-conformance-test-kit) — add `hekma-conformance` as a dev-dependency and prove your `adapter.toml` honors the contract from your own `#[test]`.
