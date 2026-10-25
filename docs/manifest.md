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

The example below is THE reference service-mode manifest — the same shape the
Conformance Test Kit's own gateway subject (`tck-gateway`) declares, and the
one the kit's end-to-end run drives through every section. A gateway is the
canonical service-mode agent: it opens a loopback HTTP port and only then is
ready, it proves it is alive by rewriting a heartbeat file, it asks for
relaunches with the conventional exit 75, and it drains cooperatively — a
pause/resume command pair plus an idle marker the drain waits on.

```toml
contract_version = "1.1.0"

[adapter]
kind = "my-gateway"
name = "My Gateway"

# Service-mode startup: open the loopback API, prove readiness through it,
# and carry the --dump delivery seam the config/memory proofs read.
[lifecycle.start]
exec = "my-gateway"
args = ["--port", "8080", "--dump", "config-dump.txt"]

# The cooperative pause/resume PAIR (both or neither): the agent stops and
# restarts accepting work under its own program — OS-independent, and it
# works even where the capability table says "unsupported".
[lifecycle.pause]
exec = "my-gateway"
args = ["--pause"]   # touches pause.sentinel in the Agent Home

[lifecycle.resume]
exec = "my-gateway"
args = ["--resume"]  # touches resume.sentinel in the Agent Home

[capabilities.pause]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[capabilities.interaction]
linux = "guaranteed"
macos = "guaranteed"
windows = "guaranteed"

[metering]
source = "self-reported"

# The readiness gate (added at 1.1.0): the start stays in `starting` — never
# `running` — until the declared probe passes.
[readiness]
kind = "http-get"
timeout_ms = 8000
url = "http://127.0.0.1:8080/models"
expected_status = 200

# The liveness watch (added at 1.1.0): while `running`, the agent rewrites
# the heartbeat; a signal stale for the whole window fails the instance.
[liveness.heartbeat_file]
path = "gateway.heartbeat"
max_age_ms = 1500

# Exit classes (added at 1.1.0): 75 is the conventional "please relaunch me"
# ask — the engine relaunches WITHOUT charging the crash loop.
[exit_codes]
restart_requested = [75]

# The drain composite (added at 1.1.0): stop-new-work → wait (bounded) for
# the idle marker → stop. A timed-out drain still stops, honestly reporting
# `completed: false`.
[drain]
timeout_ms = 10000
idle_file = "gateway.idle"

# Config delivery: the unified key → the agent's env, proven through the
# --dump seam above. The reserved "memory.dir" key declares Memory Backing
# delivery (the engine injects the managed dir under the named env var).
[config.model]
env = "GATEWAY_MODEL"

[config."memory.dir"]
env = "GATEWAY_MEMORY_DIR"
```

Every section above is cross-referenced in detail below. The five
service-mode declarations — `[readiness]`, `[liveness]`, `[exit_codes]`, the
`[lifecycle.pause]`/`[lifecycle.resume]` pair, and `[drain]` — are each
optional; a manifest that omits one simply reads `not_applicable` in the
[Conformance Test Kit](testing.md#the-conformance-test-kit) section that
would have proven it.

## Mandatory Sections

A manifest is valid only if it declares all of the following. Validation reports the first one that is missing, empty, or invalid.

### `contract_version`

The Adapter Contract version the manifest targets, as a semver string (current: `"1.1.0"`). A non-semver value is rejected. The parse is **strict `X.Y.Z`**: no `v` prefix and no partial versions (`1`, `1.0`); a prerelease or build-metadata suffix (`1.1.0-rc.1+build.5`) parses, and negotiation compares majors only. A manifest declaring an older same-major version (`"1.0.0"`) loads unchanged — same major, same schema.

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

**The optional pause/resume command pair (added at contract 1.1.0).** `[lifecycle.pause]` and `[lifecycle.resume]` are op templates like any other, and they form a PAIR: declare BOTH or NEITHER — a manifest declaring exactly one is refused at registration with both fields named (a cooperative pause the adapter cannot reverse would strand the instance paused forever). When the pair is declared, `pause`/`resume` (and the `drain` verb's step (a)) RUN THE COMMAND instead of dispatching the OS signal path — the agent suspends and resumes itself under its own program (exit 0 within the engine's 30 s standalone command cap), which is OS-independent by construction and works even where the capability table says `unsupported`. The command outranks the signal dispatch whenever it is declared — with one deliberate exception: a budget-breach pause keeps the REAL signal suspension (enforcement needs a process the engine actually froze, not an agent that was merely asked to cooperate). The transition's cause records the rendered command (`pause-cooperative` / `resume-cooperative`) — distinct from a plain `command` cause, which would read as a suspension the engine cannot verify.

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

`"http"` is documentary vocabulary (contract v1): it names where the adapter's agent really takes interaction, so an HTTP-native agent declares `interaction` supported honestly. v1 ships no engine-side HTTP delivery — the engine does not branch on the declared channel, and `hekma agent send` still writes the child's stdin; an adapter whose agent cannot read stdin should declare `interaction` unsupported on OSes where that is true (an adapter whose ONLY capability key is declared unsupported everywhere is rejected as non-viable). A real HTTP send implementation is a post-v1 change under the [versioning policy](adapter-contract.md#versioning).

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

### `[readiness]` — the readiness probe (added at contract 1.1.0)

ONE declared probe the engine must satisfy before the instance reaches `running`. Optional: a manifest without this section behaves exactly as before (the process-level startup watch alone). With it, `hekma agent start` reaches `running` only once the probe passes within its own `timeout_ms`; a timeout lands the instance `failed` with the probe **named** — and that failure is **terminal**: the Restart Policy is not consulted, because an adapter that launched but never became ready is a broken deployment a restart would only replay.

```toml
[readiness]
kind = "http-get"
timeout_ms = 5000
url = "http://127.0.0.1:8080/health"
expected_status = 200
```

Exactly ONE probe per manifest — the section is a single gate, not a pipeline. The `kind` is a closed enum: `http-get`, `file-fresh`, or `log-contains`. Each kind carries its own fields, and fields belonging to a different kind are rejected (a stray `url` under `file-fresh` names the offender — typo protection, not silent acceptance).

| Field | Kind | Meaning |
|-------|------|---------|
| `kind` | all | Which probe: `http-get`, `file-fresh`, or `log-contains`. |
| `timeout_ms` | all | The probe's whole budget, `1..=600000` ms. The **engine owns the poll cadence** within this budget (about 100 ms) — a manifest declares operator intent, never engine timing. |
| `url` + `expected_status` | `http-get` | An **`http://` URL only** (no TLS, no other schemes — the probe targets the agent's own listener on the same host) targeting a **literal loopback address** — `127.0.0.0/8` or `[::1]`, with an optional port (`1..=65535`, default 80). **Hostnames are rejected, `localhost` included**: the authority is parsed as an IP, never resolved, so the probe can never consult the machine's resolver/DNS. A `#fragment` is rejected too (a fragment never reaches the wire, so the probe would not request the path the URL names; `?query` strings are allowed). `expected_status` is the ONE status that counts as ready, `100..=599`. A connection refused before the listener is up is a plain not-yet, retried on the cadence — never an error; an endpoint that answers with the WRONG status is simply not ready, and the probe polls on until the budget runs out. |
| `path` + `max_age_ms` | `file-fresh` | A file the agent (re)writes to prove readiness, relative to the Agent Home, and the maximum modification age that counts as fresh (`1 <= max_age_ms <= timeout_ms` — a zero window can never be satisfied). The path must resolve to a regular file: a directory (or anything else) never counts as fresh. Ages are `SystemTime` deltas only: the engine compares mtimes without rounding, so the result is honest to the OS's timestamp granularity — on filesystems with coarse mtime resolution (e.g. some Windows filesystems stamp at 1–2 s granularity), a `max_age_ms` finer than that granularity cannot be honored reliably. Keep budgets comfortably above it. |
| `line` | `log-contains` | A **substring** that must appear in the engine-owned agent output log after this Run's spawn point (a prior Run's captured output stays behind the cursor — the probe only sees this Run's bytes). The engine is regex-free by design; this is a plain substring match against the **raw captured stdout** — for an ACP-transport agent that stdout carries JSON-RPC frames, so the needle must match the protocol text as actually captured (not the human-readable payload inside it). The field, like `url` and `path`, may carry **no control characters** (NUL, newlines, and the rest of C0/C1 are rejected at validation — they would corrupt the matched stream). **Under `log capture = "off"` no log exists**, so this probe is unsatisfiable there: it runs out its budget honestly and fails with the probe named — a fake pass is never manufactured. (Under `capture = "metadata"` the raw capture stays live while the instance runs, so the probe works.) |

The gate sits at the END of the start sequence, after the process-level startup watch and (for ACP transports) the session handshake — the probe is the last word before `running`. The wait happens while the start command runs (bounded by `timeout_ms`, so it can never wedge supervision), including for `--detach` starts: a detached start returns ready-or-failed. The engine emits a diagnostic naming the probe when the wait begins, another when it passes (with the wait actually taken), and a failed start's recorded cause carries the probe description plus the wait actually elapsed — visible in `hekma agent show` (`Failed cause:`) and the transition log (`readiness-timeout`).

Because the wait runs inside the engine's start path (under the supervision lock), OTHER instances' supervision pauses for up to the declared `timeout_ms` while one instance's probe waits — the same accepted bounded-wait posture as the ACP handshake. Multi-instance hosts should therefore declare SHORT probe budgets: a long `timeout_ms` delays crash detection and restarts for every co-hosted instance, not just the one being started.

### `[liveness]` — the declared liveness check (added at contract 1.1.0)

ONE declared liveness check the engine's reaper evaluates while the instance is `running`. Optional: a manifest without this section behaves exactly as before (the engine watches process liveness only). With it, the engine additionally watches the signal the adapter declares, under ONE unifying semantic: **the declared signal failing continuously for `max_age_ms` fails the instance** (`running → failed`, cause `unresponsive`, naming the check and its failing streak via `show`'s failed cause) — and the Restart Policy applies exactly as for a crash.

```toml
[liveness.heartbeat_file]
path = "heartbeat.marker"
max_age_ms = 5000
```

or

```toml
[liveness.http_get]
url = "http://127.0.0.1:8080/health"
expected_status = 200
max_age_ms = 5000
```

Exactly ONE sub-table per manifest. The shape is a closed enum of two kinds, and fields belonging to the other kind are rejected (typo protection, the same `deny_unknown_fields` posture as every section).

| Field | Kind | Meaning |
|-------|------|---------|
| `path` + `max_age_ms` | `heartbeat_file` | A file the agent (re)writes to prove it is alive, relative to the Agent Home; the engine only reads its modification age. **A missing file is failing — absent ≡ stale**: an agent that never wrote its heartbeat has never proven liveness. `max_age_ms` (`250..=600000`) is how long the mtime may lag before the heartbeat counts as stopped. |
| `url` + `expected_status` + `max_age_ms` | `http_get` | The same loopback-literal URL family the `[readiness]` probe enforces (one shared rule): `http://` only, a literal `127.0.0.0/8` or `[::1]` address, no hostnames, no DNS, no fragment. `expected_status` (`100..=599`) is the ONE status that counts as alive; any other status — or a connect/read failure — is one failing attempt, retried on the engine's reaper cadence (never an error that aborts supervision). |

`max_age_ms` (`250..=600000`, both kinds) is the failure window. A failure shorter than the window is a transient blip: the engine surfaces ONE first-failure diagnostic when the window opens (naming the check), stays silent while the blip lasts, and surfaces ONE recovery diagnostic if the signal comes back inside the window — per-tick spam is never emitted. Only a failure that persists the whole window lands the transition. A signal that FLAPS — failing and recovering inside the window over and over — gets three such pairs, then ONE note that further reopen diagnostics are suppressed (the signal is flapping); the window itself keeps never expiring while the signal recovers.

The window is enforced, not observed: the failure cannot land before `max_age_ms` has actually elapsed since the signal started failing. The engine's reaper ticks about every 250 ms, so expiry lands within `max_age_ms` plus up to ~two ticks (one to observe the failure, one to detect that the window expired) — with the 250 ms floor, no declared window is sub-observable. A manifest declares operator intent, never engine timing — declare windows comfortably above the agent's real heartbeat interval, and remember coarse-mtime filesystems (some Windows filesystems stamp at 1–2 s granularity) put a floor on honest `heartbeat_file` windows. Cost model: each reaper pass evaluates every `running` instance's declared check — there is no per-pass budget, so a host co-hosting many liveness-declaring instances pays one check per instance per tick (a per-tick budget/round-robin is tracked deferred work).

Rules that keep the check honest:

- **A paused instance is never probed.** Only an instance the store shows exactly `running` is evaluated — a suspended process cannot heartbeat (a POSIX pause holds its signals), so a pause can never fail the agent. The skip is silent by design: a pause is a normal state, not a degradation.
- **The window lives in supervision, not in the record.** A restart spawns a fresh window; an instance adopted from a previous command's engine re-derives its window fresh at adoption; a resume re-derives it fresh too — pausing clears any open failure window, so a suspension never counts against the agent. The honest consequence: after a crash-restart, an adoption, or a resume, an agent gets up to ONE extra window of grace before a persistently-failing signal fails it again.
- **Between commands a detached instance is not evaluated** (supervision is command-scoped — the same enforcement-window honesty `--detach` states at start). The declared check is evaluated by whichever command's engine holds the instance.
- **A contract 1.0.0 engine rejects the section** at parse time, naming it (the older-same-major-engine rule, for free — see [the Adapter Contract](adapter-contract.md#versioning)).

### `[exit_codes]` — the declared exit-code classes (added at contract 1.1.0)

Two optional lists giving the agent's own exit codes DECLARED MEANING. Optional: a manifest without this section behaves exactly as before — every unrequested exit is a crash. With it, the engine classifies a coded exit out of `running` against the declared classes and acts on the MEANING, not just the fact:

```toml
[exit_codes]
restart_requested = [75]
clean = [0]
```

| Field | Meaning |
|-------|---------|
| `restart_requested` | Exit codes by which the agent ASKS its supervisor for a relaunch (the conventional exit-75 pattern). The exit takes the crash-path dance — the instance lands `failed` with the `restart-requested` cause naming the declared class AND the recycle depth (`requested-recycle N`, so the pacing is visible in `show` and the log), and the Restart Policy's backoff paces the relaunch — but the consecutive-failure counter is **NOT charged**: the agent's own recycle proves neither stability nor failure, so the counter simply holds. |
| `clean` | Exit codes by which the agent FINISHES ON PURPOSE. The instance lands `stopped` (never `failed`) with the `clean-exit` cause, the record settles exactly like a requested stop, and NO restart follows — the operator's next fresh start is the counter reset, exactly as it has always been. |

Each list is optional (an empty section declares nothing and classifies nothing). Validation: every code must be in `0..=255` (the portable exit-code domain — unix exit status carries the low 8 bits, so a Windows code above 255 simply never matches a declared code); a code appearing in BOTH lists is refused at registration (both fields named); duplicates within one list are refused.

Rules that keep the classes honest:

- **The classes classify RUNS, not launches.** Only a CODED exit out of exactly `running` is classified. An exit during `starting` (the survival window, a readiness probe, the ACP handshake) is a launch failure on its own path — never classified, and it still CHARGES the consecutive-failure counter exactly as it did before this section existed (a declared code does not soften a launch failure: the agent never reached `running`). A signal termination carries no code and is never classified. A crash out of `paused` rides the plain crash path.
- **`never` outranks the ask.** Under `Restart Policy: never` a `restart_requested` exit still lands terminal `failed` with NO restart — the cause names both the declared class and the policy verdict ("the operator's policy outranks the adapter's ask").
- **A restart-requested loop NEVER trips the crash-loop terminal.** This is the point of the class and the stated operator contract: the operator DECLARED the code a relaunch request, so charging the crash loop toward a 5-failure terminal would punish exactly the behavior they blessed. The relaunches keep coming — but the backoff still grows with the recycle streak to its cap (60 s), so the RATE is bounded even though the terminal is unreachable. (The durable crash counter is never touched by the chain; the streak paces only the delay.) The streak is IN-MEMORY: an engine restart forgets it (the next recycle waits the base again — the durable counter is the operator's signal), and it clears at every fresh operator start, stop, clean exit, and CHARGED crash — a stable-window reset forgives the pacing history along with the failure count, so a post-reset chain starts at `requested-recycle 1`.
- **The stability anchor.** The engine separately tracks how long a run has held `running` continuously: a run that crashes after ≥ 30 s of stable running resets the consecutive-failure counter first, so the crash charges as failure #1 (the reset is named in the crash cause: "the previous run was stable for Ns — consecutive-failure count reset"). The measurement is tick-granular — the anchor is read at the reaper's poll cadence (~250 ms), so a crash's measured run length can read up to one reaper tick longer than the process actually ran (±1-tick slop, never a shorter read). The window is ENGINE-owned (30 s — not adapter surface), a pause interval never counts toward it (the anchor restamps at every resume — the resumed run re-proves the whole window), and an adopted instance's anchor starts at ADOPTION: its pre-adoption run time is not observable, so its crash resets only after the adopting engine has itself watched the run for the whole window. A fresh operator start resets the counter too, exactly as it always has.
- **Adopted instances: the classification split by platform.** On unix an adopted process is not the engine's child, so its exit code is unrecoverable (the crash cause says so) — no code, no classification, on any manifest. On Windows the adopted poll DOES read the real exit code (`GetExitCodeProcess`), so a declared code classifies there — but only when the classes RESOLVED at adoption: adoption re-resolves the section from the adapter's own manifest, and a drifted-unresolvable section is refused with ONE diagnostic ("exit codes are NOT classified this Run — stop and start the instance to restore the declared classes") while the adoption itself succeeds — every unclassified exit is a crash, the pre-section behavior, until a stop and start restores the classes.
- **A contract 1.0.0 engine rejects the section** at parse time, naming it (see [the Adapter Contract](adapter-contract.md#versioning)).

### `[drain]` — the drain verb's declared wait (added at contract 1.1.0)

The optional section behind `hekma agent drain <name>` — the engine's **drain** composite: stop new work (the adapter's declared pause command), wait — BOUNDED — for the agent's in-flight work to reach zero, then stop through the ordinary ladder. Optional: a manifest without this section takes the engine's default drain posture (30 s bound when a pause command is declared; a direct stop when there is nothing declared to wait for).

```toml
[drain]
timeout_ms = 30000
idle_file = "idle.marker"
```

| Field | Type | Required | Meaning |
|-------|------|----------|---------|
| `timeout_ms` | integer | yes | The declared drain bound, `1..=600000` ms (out-of-range is refused at parse, naming the field). The ceiling on the WHOLE bounded phase — the pause command AND the idle wait share it. |
| `idle_file` | string | no | A file the agent touches when its in-flight work reaches zero, relative to the Agent Home. The engine reads its modification freshness against the wall clock with a 2 s engine-owned freshness slack (a regular file modified within the last 2 s counts as fresh — the slack absorbs poll-cadence slop); absent ≡ stale, stale ≡ not idle, and a future mtime (clock skew) reads as age zero. Omit it when the pause command's exit alone is the wait. |

Rules that keep the composite honest:

- **The composite is pause (command) → wait (bounded) → stop.** Step (a) runs the declared `[lifecycle.pause]` command under the drain's own bound — a command that exits nonzero or cannot spawn REJECTS the drain with the durable state untouched (never a fake pause), while a command that outlives the bound is killed, which IS the timed-out path (the drain still stops). Step (b) polls the declared `idle_file` (when declared) against the same deadline, and polls the agent process itself alongside it: a child that ends on its own mid-wait (a crash or a declared exit) breaks the wait early — the drain proceeds to its stop with the honest not-completed outcome rather than burning the whole bound over a corpse. Step (c) is the ordinary stop ladder — unconditional.
- **A timed-out drain STILL stops, honestly.** The bound is a ceiling on the wait, never on the stop: when it burns first the instance is stopped anyway and the outcome is recorded as `completed: false` on the terminal `drain-stopped` cause, with `waited_ms` the measured wall clock the phase actually spent. Both outcomes are command SUCCESSes — the CLI prints the report and exits 0, noting a timed-out drain on stderr.
- **The bound is three-tier.** The CLI's `--timeout-ms` override wins; else the manifest's `timeout_ms`; else the engine default (30 s, only reachable when a pause command is declared — with neither a command nor an idle signal there is nothing to wait for).
- **Two different 30 s bounds exist — do not conflate them.** A STANDALONE `pause`/`resume` verb runs its declared command under the engine's standalone command cap (30 s, engine-owned — an overrun kills the command and REJECTS the verb, exit 1). The SAME command inside a drain runs under the DRAIN's own bound instead (the three tiers above; 30 s there is only the last-tier default) — an overrun inside a drain is the timed-out path, not a rejection. The manifest cannot set either cap directly; only the drain bound is influenceable, via `timeout_ms` (or the CLI override).
- **The drain wait holds the supervision lock.** Like the start path's probe waits, the whole bounded phase runs under the instance's supervision lock — so OTHER instances' reaper-driven supervision (readiness probes, liveness checks, crash detection) pauses for up to the drain's bound while one drain waits. Multi-instance hosts should declare SHORT drain bounds for the same reason [readiness probes](#readiness--the-readiness-probe-added-at-contract-110) are kept short.
- **Freshness is a wall-clock read.** The idle check compares the file's mtime to the system clock at poll time; an NTP back-step (or any large clock adjustment) can transiently distort the read either way — the 2 s slack and the drain's own bound are what contain it, and the check recovers on the next poll once the clock settles.
- **The degenerate shape stops directly.** No pause command AND no idle file means there is no declared wait at all: the drain stops immediately, records `completed: true, waited_ms: 0`, and says so with a diagnostic — surfaced, not silent.
- **Drain is a `running`-only verb.** A registered (never started) or paused instance refuses the drain naming its state; a bound override outside `1..=600000` ms refuses naming the bound. The state is untouched by a refusal.
- **The commands pair is required for the pause step.** Drain runs the `[lifecycle.pause]` command from [the lifecycle pair](#lifecycle) — a manifest cannot declare `[drain]` semantics that depend on a command it does not declare. Without the pair, the drain is a bounded idle wait (or the degenerate direct stop) followed by the stop.
- **A contract 1.0.0 engine rejects the section** at parse time, naming it (see [the Adapter Contract](adapter-contract.md#versioning)).

## See Also

- [Adapter Contract](adapter-contract.md) — the versioned contract this manifest speaks: negotiation, versioning/deprecation policy, and the ratified v1 decisions.
- [Command reference](commands.md) — the `hekma agent` commands and unified config keys.
- [Architecture](architecture.md) — the Adapter Contract, the Usage Ledger, and budget enforcement.
- [The Conformance Test Kit](testing.md#the-conformance-test-kit) — add `hekma-conformance` as a dev-dependency and prove your `adapter.toml` honors the contract from your own `#[test]`.
