---
title: Control API (v0)
description: The HTTP/JSON surface served from `hekma serve` — the local endpoint the CLI routes through, the operator listener and its TLS rule, bearer auth, routes, the SSE and cursor-pull event contracts, and the v0 stability marker.
---

# Control API (v0)

The Service Host serves a local HTTP/JSON surface over the agents it
supervises: registration, lifecycle, configuration, budgets and usage (through
the Fleet row), input, memory, and live events. It requires a bearer token on
**every** route, and it is versioned `/v0` and marked **unstable** — the
surface freezes only after a second hosting platform validates it, so fields
and error wording are additive-only until then and may still change.

## The local endpoint (every serve)

Every `hekma serve` binds a **loopback local endpoint** — `127.0.0.1`, an
OS-chosen port, plain HTTP — and records it in `<state-dir>/control.json`:

```json
{
  "schema_version": 1,
  "pid": 4242,
  "addr": "127.0.0.1:53113",
  "token": "<64 hex digits>",
  "hekma_version": "0.11.0",
  "autostarted": false
}
```

The `token` is a fresh 256-bit operator token minted from the OS random source
for each serve. The file is written owner-only (`0600` on Unix; on Windows it
inherits the state directory's ACL — the same trust boundary as
`secrets.toml`), written to a temporary file and renamed into place after the
bind, and removed when the API stops at shutdown. The host prints a
`local control endpoint on <addr> …` notice on stderr.

This is how the `hekma` CLI reaches the host that owns a state directory:
every `hekma agent` write is one call to this endpoint, and when no host
serves the directory the first write starts one in the background (see
[how the CLI reaches the Service Host](commands.md#how-the-cli-reaches-the-service-host)).
A host started that way is an **auto-started** host (`"autostarted": true`):
it serves only the local endpoint, ignores SIGHUP (closing the terminal does
not end it), and drains and exits once no agent is `starting`, `running`,
`paused` or `stopping` and no request has arrived for 30 seconds — or when
its state directory is deleted. An explicit `hekma serve` never idle-exits.

## The operator listener (`--listen`)

```bash
CONTROL_BEARER="s3cret" hekma serve --listen 127.0.0.1:8477 --bearer-secret CONTROL_BEARER
```

- `--listen <addr>` — an additional address to serve the API on (a socket
  address like `127.0.0.1:8477`). Default: off — only the local endpoint
  exists.
- `--bearer-secret <name>` — the NAME of the operator's bearer secret.
- `--tls-cert <pem>` + `--tls-key <pem>` — serve `--listen` over **HTTPS**
  with this PEM certificate chain and private key. **Required for a
  non-loopback address.**

`--listen` and `--bearer-secret` are validated together: **both or neither** —
a half-configured API is a usage error before anything starts, and so is an
unparseable address. The secret NAME resolves at startup through the same
composite resolver every Hekma secret uses: the environment variable with
that exact name first, then a key in the state root's `secrets.toml` (which
must not be group/other readable). Two resolved shapes refuse the startup
with a typed error before anything binds or opens: an **unresolvable** name
(the API never serves unauthenticated) and a secret that resolved to an
**empty value** (an empty bearer could never authenticate — the error names
the reference to fix). When the listener is up, the host prints a
`control api listening on <addr> (v0, unstable — bearer required; http|https)`
notice on stderr; that notice is the address of record (with port `0`, it
carries the OS-chosen port).

**Beyond loopback, TLS is required.** A `--listen` address that is not
loopback (for example `0.0.0.0:8477`, the container recipe) without
`--tls-cert` and `--tls-key` is refused with exit code `2` before the host
takes its lock, binds or opens anything. The two TLS flags go together (one
alone is a usage error) and need `--listen`; the PEM files are read when the
host starts, and an unreadable file or a key that does not match the
certificate is refused the same way, naming the path. With both, the listener
answers HTTPS only — a plain-HTTP request on it fails the TLS handshake and is
never served. A loopback `--listen` may use TLS too; the local endpoint never
does. The TLS stack is the vendored rustls + ring pair the engine already
ships — no system TLS library. For the full container posture — the static
build, the stop-grace mapping, the volume-backed state — see
[Running Hekma as a container supervisor](hosting.md).

## Auth

Every route on every listener — `/v0/version` included — requires
`Authorization: Bearer <token>`. Credentials go through one **credential
resolver**: it holds the SHA-256 of each credential with its **class**, hashes
the presented value and compares it in constant time against every entry, then
checks the class against the authority matrix for the surface. Today one class
is issued — the **operator/Host bearer** — and it carries two credentials: the
local endpoint's token (from `control.json`) and the `--bearer-secret` value.
Both work on both listeners and on every route. The other credential classes
(announce-scoped credentials, Run Keys, refresh credentials, Node
credentials) arrive with their stories; a credential whose class a surface
does not allow will answer a typed `403 forbidden` (reserved — no issued
class can reach it yet).

A missing or wrong credential — including several `Authorization` fields, a
scheme other than `Bearer`, or an empty value — answers `401` with a
`WWW-Authenticate: Bearer` challenge and a static refusal body that never
echoes the presented token. No handler runs for an unauthenticated request,
and no secret VALUE ever enters a response, a log line, or an SSE frame — the
API projects no secret reveal at all (a `secret:NAME` config value renders
masked, exactly as the CLI displays it).

## Routes

| Method | Path | Purpose | Success |
|--------|------|---------|---------|
| `GET` | `/v0/version` | Self-description: identity, `v0`/`unstable` marker, and the engine schema versions this build projects | 200 |
| `GET` | `/v0/fleet` | Every instance's Fleet row (the `list --json` document, verbatim) | 200 |
| `POST` | `/v0/instances` | Register from exactly one adapter source: `{"name","kind"}` or `{"name","manifest"}` (an ABSOLUTE `adapter.toml` directory path) — both or neither is a 400 | 201 + `Location: /v0/instances/{name}` + read-back |
| `GET` | `/v0/instances/{name}` | One instance's Fleet row (the `show --json` document) | 200 |
| `DELETE` | `/v0/instances/{name}?disposition=&force=` | Remove an instance. `disposition` is `retain` (default — the Agent Home stays on disk) or `delete`; `force=true` removes a running instance (without it a running one is a 409 — stop it first): the instance is first stopped through the bounded stop ladder with the default 30 s window, so the request can take that long. On Windows, where no graceful signal is sent yet, the whole window elapses before the forced kill. Any other value is a 400 | 204, empty |
| `GET` | `/v0/instances/{name}/status` | Runtime status: the registry row, effective restart policy, restart count, failure cause | 200 |
| `GET` | `/v0/instances/{name}/capabilities` | The effective Capability Declaration projected onto this OS | 200 |
| `GET` | `/v0/instances/{name}/config` | The effective config (the `config get --json` document; values masked) | 200 |
| `PUT` | `/v0/instances/{name}/config` | Set one key: `{"key","value"}` — acknowledged with the post-write read-back plus the engine's warn-only steering warnings | 200 |
| `GET` | `/v0/instances/{name}/events` | Cursor pull: the engine's resync batch for one instance (lossless + idempotent) | 200 |
| `POST` | `/v0/instances/{name}/start` | Start the agent. Optional body `{"detach": true}`: the handle is disarmed at spawn, so the agent also survives this host's exit (an `engine-observed` instance is refused). A PRESENT body must parse | 200 + read-back |
| `POST` | `/v0/instances/{name}/stop` | Stop through the bounded ladder; optional body `{"window_secs": N}` (1..=86400, out-of-bounds is a 400). A PRESENT body must parse — only a truly ABSENT body means the engine's default window | 200 + read-back |
| `POST` | `/v0/instances/{name}/pause` | Pause the agent | 200 + read-back |
| `POST` | `/v0/instances/{name}/resume` | Resume a paused agent | 200 + read-back |
| `POST` | `/v0/instances/{name}/drain` | Drain the agent (pause command → bounded idle wait → stop); optional body `{"timeout_ms": N}` (`1..=600000`, out-of-bounds is a 400; a truly absent body means the manifest tier, else the engine default). BOTH outcomes are `200` — a timed-out drain still stopped the agent, and the document reports it honestly (`completed: false`, with `waited_ms` the measured wait) rather than as an error. Draining a non-running instance is refused with the lifecycle verbs' 409 conflict, and so is a declared pause command that fails (nonzero exit / spawn failure / overrun — the adapter refused the verb, the agent is untouched, so it is a retryable 409 `conflict` carrying the rendered command, never a 500). The response is `{"schema_version", "instance", "completed", "waited_ms"}` | 200 + read-back + outcome |
| `POST` | `/v0/instances/{name}/send` | Send `{"text": "…"}` to the running agent's input channel. The host holds the input pipe of every agent it started, so this reaches an agent another client started | 204, empty |
| `PUT` | `/v0/instances/{name}/memory` | Attach a Memory Backing: `{"kind": "filesystem"}` or `{"kind": "native"}` (terminal states only). The response is `{"schema_version","instance","dir","kind","guarantee","declared"}`; if the read-back fails after the attach committed, it is still a `200` with the three read-back fields `null` and `readback_error` naming the failure | 200 |
| `DELETE` | `/v0/instances/{name}/memory` | Detach the Memory Backing (metadata only — the managed directory stays on disk) | 204, empty |
| `PUT` | `/v0/instances/{name}/restart-policy` | Set the Restart Policy: `{"policy": "never"}` or `{"policy": "on-failure"}` — acknowledged with the runtime status read-back | 200 |
| `GET` | `/v0/events` | The live SSE stream (below) | 200, stays open |
| `GET` | `/v0/export` | The global outbox pull (below): ONE bounded batch of committed events in ONE global order, ack-cursor-resumable | 200 |
| `POST` | `/v0/export/ack` | Advance the durable export ack cursor: `{"seq": N}` — monotonic and idempotent, the cursor as persisted comes back | 200 |
| `POST` | `/v0/runs` | Open a child Run (below): exactly one of `parent_run_id` or `parent_instance`, plus optional `labels`, `token_budget`, `dollar_cap`, `breach_action` and `external_id` | 201 + `Location: /v0/runs/{id}` + the Run |
| `GET` | `/v0/runs` | The Run listing (below): `?selector=&view=exceptions\|all&limit=&cursor=` — the same document `hekma ps --json` prints | 200 |
| `GET` | `/v0/runs/{id}` | One Run: its row, labels, limits, subtree usage and open children | 200 |
| `POST` | `/v0/runs/{id}/close` | Close a child Run (idempotent; a Run that owns an agent's process is a 409 — stop the instance) | 200 + the Run |
| `GET` | `/v0/runs/{id}/labels` | One Run's labels, bound and caller, each with its `source` and (bound) `origin` | 200 |
| `PATCH` | `/v0/runs/{id}/labels` | Bind and unbind labels as the Operator (below): `{"set": {…}, "remove": […], "dry_run": bool}` | 200 + the labels after |
| `GET` | `/v0/policies` | Every Policy, `default` included, ordered by name | 200 |
| `GET` | `/v0/policies/{name}` | One Policy: its Selector, limits and (on `default`) its model-keyed Rate table | 200 |
| `POST` | `/v0/policies` | Create a Policy: `{"name", "selector", "tokens_per_run"?, "tokens_cumulative"?, "dollars_per_run_micros"?, "dollars_cumulative_micros"?, "breach_action"?, "rate"?}` — answers the matched and fallback counts | 201 + `Location: /v0/policies/{name}` |
| `PATCH` | `/v0/policies/{name}` | Change a Policy: `{"selector"?, "set": {limits}, "clear": […], "rates": {"set": {model: rate}, "remove": [models]}, "dry_run": bool}` | 200 + the Policy after (or as planned) |
| `DELETE` | `/v0/policies/{name}` | Delete a Policy; its matched Runs re-resolve under what remains (`default` is refused) | 204 |

The identity-bearing reads return the engine's own documents — the same wire
shapes (and schema versions) the CLI already renders. A `POST` lifecycle verb
answers with a fresh `GET`-style read-back of the instance, so a client never
has to guess what the verb actually did. Registration is committed-truth: the
`201` (with its `Location` header) answers as soon as the registration is
durable; the read-back body is the courtesy payload, and in the rare race
where the instance is removed again before the read-back runs, the `201` +
`Location` still stand with an empty body — the client resolves the truth at
the `Location`. A `PUT .../config` is serialized per host (write + read-back
under one lock), so a response's read-back always reflects the value ITS
request wrote; `GET`s stay unsynchronized snapshots.

## Runs

A Run is one unit of agent work with a stable run id (see
[Runs](runs.md)). Every started instance owns one; a Host opens **child
Runs** under it for its own units of work:

```text
POST /v0/runs
{
  "parent_instance": "my-agent",
  "labels": {"task": "review"},
  "token_budget": 20000,
  "dollar_cap": "1.50",
  "external_id": "job-42"
}
```

`parent_run_id` names any open Run instead of an instance. `labels` are caller
labels. `token_budget` and `dollar_cap` (a dollar string) are per-run ceilings
over the child and its own children; they take `breach_action` `warn` (the
default) — `pause`, `stop` and `block` are refused for a child Run today.
A retried request with the same `external_id` answers `201` with the same
Run. The response (and `GET /v0/runs/{id}`) is
`{"schema_version": 1, "run": {…}}`: the run id, `control_depth`,
`wire_path`, `status`, `anomalies`, `parent_run_id`, `instance`,
`process_owner`, `end_cause`, timestamps, `labels` (each with its `source`),
`limits`, `usage` (input, output and cached tokens plus `cost_micros`, over the
Run's subtree, labelled `estimated`) and `open_children`.

Run refusals carry a stable `error.code` beside the `kind`:

| `code` | Status | When |
|--------|--------|------|
| `run-not-found` | 404 | No Run has the id |
| `no-open-run` | 409 | `parent_instance` has no open Run (start it first) |
| `parent-ended` | 409 | The parent Run has ended |
| `owns-process` | 409 | `close` on a Run that owns an agent's process |
| `nesting-too-deep` | 400 | The child would nest more than 8 levels deep |
| `action-unavailable` | 400 | The Breach Action cannot execute at the child's depth; the message names the depth and the Metering Source |
| `invalid-request` | 400 | A malformed label or field |
| `run-ended` | 409 | The Run has ended, so its labels cannot change |
| `label-from-config` | 409 | `remove` names a label the agent's `labels.<key>` configuration binds; the message names the key and how to change it |
| `label-not-bound` | 409 | `remove` names a label the Run set about itself (a caller label) |
| `label-bound` | 409 | A caller label names a key already bound on the Run |
| `instance-not-found` | 404 | No agent has the name given as a label target |

### The Run listing

`GET /v0/runs` is `hekma ps`'s read — every filter, Selector and page
parameter of the CLI has its equivalent here. Query parameters:

| Parameter | Meaning |
|---|---|
| `view` | `exceptions` (the default) or `all` |
| `selector` | `key=value[,key=value…]` — bound and caller labels alike |
| `limit` | Page size, default 50, at most 1000; `0` means the maximum, above the cap it clamps |
| `cursor` | The opaque cursor a previous page's `next_cursor` printed |

The answer is `{"schema_version": 1, "view", "selector", "items",
"next_cursor", "counts"}` — `items` byte-shape-equal to the CLI's `--json`
items. Each item flattens the Run's row (`run_id`, `control_depth`,
`wire_path`, `status`, `anomalies`, `instance`, timestamps, …) and adds
`labels` (each with `source` and bound `origin`), the subtree `usage`, the
instance `cumulative_usage` on process-owning rows only (absent on child
rows — a shared pot is not a per-Run figure), `latched` breaches as
`[{dimension, scope, action}]`, a `budget` headroom block against the
Run's **resolved** limits (the `BudgetView` field names plus the per-limit
`sources` and the `effective_action` after the depth fallback; dollar
figures only where a Rate prices them — an unpriced cap shows the
`unpriced-model` Anomaly instead), and `kept_on_pause` where the
breach-default upgrade holds the agent on `pause`. `counts` carries
`listed` (what the active view matches) and `healthy_open`. `next_cursor`
is `null` exactly when the read is exhausted.

Malformed parameters are typed `400`s with their own `error.code`:
`invalid-selector`, `invalid-cursor` (a cursor this listing never minted —
including one from a future format), `invalid-limit` and `invalid-view`.

### Bound labels

`PATCH /v0/runs/{id}/labels` binds and unbinds labels on one Run with the
Operator's bearer — the same verb as `hekma label` (see
[Runs](runs.md#bound-labels)):

```text
PATCH /v0/runs/run_01JZ6Q9G3S8V2X7K4M1N5P0R2T/labels
{"set": {"team": "core"}, "remove": ["tier"], "dry_run": false}
```

A label in `set` is bound with origin `label-verb`; it outranks the agent's
`labels.<key>` configuration and any caller label on the same key. A key in
`remove` takes back a label set this way, and if the agent's configuration
binds the same key, its value returns (origin `config`) in the same change. An
absent key is unchanged. The request is all or nothing; `dry_run` answers with
the labels the change would leave and writes nothing. The answer is
`{"schema_version": 1, "run_id", "labels", "prior_labels", "changed",
"dry_run"}`: the Run's labels after (or as planned) and before, bound and
caller. An empty change, a malformed key or value, or a key both set and
removed is a `400 invalid_body`. Unlike the other bodies, an unknown field is
refused (`422 invalid_body`), so a misspelled `dry_run` never becomes a real
write. A change commits one `labels-changed` Run
event; repeating it changes nothing and emits nothing.
`GET /v0/runs/{id}/labels` answers `{"schema_version": 1, "run_id", "labels"}`.

## Documents and versions

Every JSON payload carries `schema_version`, and the stamping is honest about
its family. Documents that wrap an engine payload (the Fleet row, the config
reads) reuse the engine's own version — the same number the CLI's identical
shape carries. Documents that are Control-API originals (`/v0/version`, the
status and capabilities wrappers) stamp the Control-API document family
(currently 1) under the `unstable` marker. A client checks the marker, not a
changelog, to know what it is talking to:

```json
{
  "api": "hekma-control-api",
  "version": "v0",
  "stability": "unstable",
  "schema_version": 1,
  "host_version": "0.1.0",
  "engine": {
    "event": 1,
    "fleet": 2,
    "usage": 1,
    "budget": 1,
    "log": 1,
    "run": 1
  },
  "log_capture": "full"
}
```

`host_version` is a top-level field — it describes the HOST build you are
talking to; the `engine` object carries only the engine's payload-family
versions. `log_capture` names the engine's capture posture (`full`,
`metadata`, or `off`) — the self-description that tells you what
`output.log` holds and which capture surfaces serve, without a failed
round-trip.

## Errors

Every error is a versioned JSON body — never a bare status, on any failure
class:

```json
{ "schema_version": 1, "error": { "kind": "not_found", "message": "…" } }
```

The Run routes add a finer `"code"` inside `error` (see [Runs](#runs)); every
other route omits it.

When the failure is an engine error, the `error` object also carries the
engine's **typed** error — the additive `engine_error` field — so a client
that links the engine (the `hekma` CLI does) rebuilds the engine's own error
value and renders exactly what it would have rendered in-process:

```json
{
  "schema_version": 1,
  "error": {
    "kind": "not_found",
    "message": "no Agent Instance named 'ghost' is registered",
    "engine_error": {
      "schema_version": 1,
      "family": "engine",
      "variant": "NotFound",
      "fields": { "name": "ghost" }
    }
  }
}
```

`family` is `engine`, `registry` or `config`; `variant` and `fields` mirror
the engine's error type (nested parts — a store or process-control cause —
are `{"variant","fields"}` objects; an I/O cause is carried as its message).
A client that does not know a variant falls back to `kind` and `message`.

That promise covers the ROUTING and EXTRACTION failures too: an unmatched path
is an envelope `404` (`not_found`), a known path with an unsupported method is
an envelope `405` (`method_not_allowed`), and a body the extractors reject
answers the envelope with the rejection's own status — `400` for unparseable
JSON, `415` for a body without a usable content-type, `422` for JSON that
parses but does not fit the route's type. All of them sit behind the auth
layer: without a valid bearer, any path answers `401` first.

| `kind` | Status | When |
|--------|--------|------|
| `unauthorized` | 401 | Missing or wrong bearer (with the `WWW-Authenticate: Bearer` challenge) — including an ambiguous request carrying MULTIPLE `Authorization` headers (refused, never first-wins; the scheme itself is case-insensitive per RFC 9110) |
| `forbidden` | 403 | Reserved: a valid credential whose class the surface does not allow (no issued class reaches it today) |
| `invalid_body` | 400 | A body/query failed validation: malformed JSON, `kind`+`manifest` together, `window_secs` out of bounds, malformed cursor, a RELATIVE `manifest` path, an unknown memory kind or restart policy, a `disposition`/`force` value outside its set |
| `invalid_body` | 415 / 422 | A body the extractors reject: no usable content-type (415), or JSON that does not fit the route's type (422) |
| `invalid_name` | 400 | The instance name is not a legal name |
| `not_found` | 404 | The instance does not exist — or the path matches no route |
| `method_not_allowed` | 405 | The path exists but the method does not |
| `conflict` | 409 | The action contradicts the instance's state (stop on a stopped instance; remove a running one) — or the adapter's own lifecycle command refused it (a pause/resume/drain command that exits nonzero, cannot spawn, or overruns its bound; the instance's state is untouched and the error names the rendered command) |
| `cursor_invalid` | 409 | The cursor contradicts the record: the per-instance pull's cursor PAST the family's committed count, an export cursor below the outbox's retained floor or PAST its committed tail, or an export ack past the tail — the engine's truncation/retention guards, surfaced verbatim (never a clamp) |
| `internal` | 500 | Something failed on the host side; the message names it |

## Events

Two complementary surfaces, composed the engine's documented way — **subscribe
first** (the SSE stream), then act, then pull the durable record.

**Live — `GET /v0/events` (SSE).** One engine event bus subscription per
connected client; each event rides one `data:` frame whose payload is the
engine's own kind-tagged JSON (`{"kind":"transition",…}`,
`{"kind":"budget_breach",…}`, `{"kind":"usage_update",…}`, and
`{"kind":"run",…}` for a Run created, ended or changed) — never re-shaped.
Every event frame also carries `id: <seq>` — the event's position in the
global outbox, the same `seq` `GET /v0/export` serves. A client that falls
behind the bus gets an in-stream `{"kind":"lagged","skipped":N}` frame
naming the gap and the stream continues at the current tail (the skipped
events stay readable in the durable logs — the cursor pull is the lossless
heal; lag is surfaced, never silent; the `lagged` frame carries no `id` —
it is a gap, not an event). The stream ends when the engine is gone **or
the API shuts down** — a client sees the connection close and can
reconnect; the engine is unaffected. (The end fires even if the shutdown
already fired before the stream's first poll — a late subscriber is never
left hanging.)

**Resume — `Last-Event-ID: <seq>`.** A reconnect that presents the last id
it saw is validated BEFORE streaming: a seq below the outbox's retained
floor answers `409 cursor-below-retention`, one past the committed tail
`409 cursor-past-tail` (typed, never clamped — the export contract's
rule). The server then subscribes first, replays the outbox rows with
`seq > N` through the active Selector filter, and hands over to live
delivery dropping live seqs the replay already delivered — no duplicate,
no gap at the seam.

**Filter — `?selector=key=value[,…]`.** With a Selector only events whose
subject Run currently carries every term (bound or caller labels alike)
are delivered: Run events by their run id, usage and breach events by
theirs, a transition by its instance's open process-owning Run. Events
that name no Run (`policy`, `breach-default-upgraded`) are delivered only
without a Selector. The filter reads the subject Run's CURRENT labels —
the in-memory snapshot first, one read-only store read when the Run has
ended (ended Runs keep matching until compaction). Honest limitation, by
design: labels-at-event-time is not stored anywhere, so the filter cannot
see it. A failed filter read surfaces a `{"kind":"filter-error",…}` frame
(no id) and drops that one event — surfaced, never a silent skip. A
malformed `selector` is the `400 invalid-selector` envelope.

**Durable — `GET /v0/instances/{name}/events?cursor=…`.** The engine's
`resync` batch for one instance, verbatim: family-major, cursor-lossless, read
from one database snapshot (`torn_tail_skipped` stays in the payload and is
always `false`). The
first pull may omit the cursor (from the beginning); every batch carries the
cursor to present next. Replaying a consumed cursor is idempotent, and a
cursor PAST the family's committed count is a `cursor_invalid` 409 —
the engine refuses rather than guess.

## The global outbox export (for ephemeral hosts)

The per-instance pull above heals one instance's family. A Host on an
**ephemeral disk** (a container that dies with the machine) needs the other
half: one durable record of EVERY committed event — transitions, budget
breaches, usage updates, across ALL instances — that survives the host's own
death, is bounded, and can be consumed with an acknowledgement. That is the
engine's global outbox: a table in the state database, written in the same
transaction as each committed transition, breach and usage update (so it
never holds an event that did not commit, and never misses one that did), in
commit order, which is also the event bus order. Every record carries a
`seq` that only ever increases, across restarts and across every engine
process that shares the state directory.

**Pull — `GET /v0/export?cursor=&limit=`.** One bounded batch of outbox
entries in seq order:

```json
{
  "schema_version": 1,
  "events": [
    {
      "schema_version": 1,
      "seq": 7,
      "event": {
        "kind": "transition",
        "schema_version": 1,
        "instance": "worker",
        "prior_state": "starting",
        "new_state": "running",
        "cause": { "kind": "adapter-ready" },
        "at": "2026-10-19T12:00:00Z"
      }
    }
  ],
  "cursor": { "seq": 7 },
  "torn_tail_skipped": false,
  "blank_lines_skipped": 0,
  "retained_from": 1
}
```

- **The default pull resumes from the durable ack.** With NO `cursor` query
  parameter you get everything AFTER the highest acked seq — the
  give-me-whats-new pull. From-zero backfill is explicit:
  `?cursor={"seq":0}` (URL-encode the JSON). A malformed cursor parameter is
  a `400 invalid_body`.
- **`events` carries entries with `seq` strictly greater than the request
  cursor**, bounded by `limit` (default 500, hard cap 10 000 — larger values
  are clamped, never refused). `limit` must be a positive whole number
  (`>= 1`): a non-numeric, negative, or oversized value — and `limit=0`,
  which would otherwise silently deliver the default batch — is a
  `400 invalid_body` inside the versioned envelope. An empty `events` array
  means you are caught up; the batch's `cursor` is the position to hand to
  the next pull (or to the ack).
- **`retained_from`** is the seq of the outbox's first RETAINED entry. The
  outbox is bounded (compacted at append time): the acked prefix is
  dropped first, and only if the cap is still crossed the oldest UN-ACKED
  entries too — never the very last entry (the cap counts stored event
  bytes, 30 MB), and every un-acked eviction is announced on the host's stderr.
  A Host that pulls regularly never loses an un-acked event; a Host that
  goes silent past the cap does, honestly.
- **`torn_tail_skipped`** and **`blank_lines_skipped`** stay in the payload
  for compatibility with the earlier file-backed outbox and are always
  `false` and `0`: the outbox lives in the state database, where a crash
  cannot tear a record.

**Refusals — `409 cursor_invalid`, never a clamp.** A cursor below the
retained floor (its entries were rotated away — serving it would silently
skip them; the lossless rule: anything at `retained_from - 1` or above is
servable, because its window starts at the first retained entry) and a
cursor past the committed tail (the outbox never committed that far) are
both typed 409s whose message names both counts. Reset to
`{"seq":0}` deliberately if losing the rotated-away prefix is intended —
the engine never guesses.

**A pre-upgrade journal that cannot be read is loud.** If the one-time
import finds an `outbox.jsonl` it cannot verify (an unparseable
newline-terminated line, a wrong-shape record, or a duplicate/decreasing
`seq`), the engine refuses to open rather than mint seqs from an unverified
history. To recover: quarantine the file by renaming it, open the engine
again (the outbox then starts above the ack cursor), and re-sync the families
with the per-instance `events` pull, which remains the complete heal.

**Acknowledge — `POST /v0/export/ack`** with `{"seq": N}`: everything at or
below `N` is consumed and becomes compaction-eligible. The ack is
MONOTONIC (`max(current, given)` persisted) — a re-ack and a backward ack
are no-op successes that return the cursor AS PERSISTED, so a client can
read back its own idempotence:

```json
{ "schema_version": 1, "seq": 7 }
```

An ack past the journal's committed tail is the same `409 cursor_invalid`
refusal — an ack is a claim about entries that exist. The ack cursor is
durable (stored in the state database with the outbox): it survives a host
restart, and the default pull resumes from it.

**Ordering, stated plainly.** Seq order is commit order, for every engine
process sharing the state directory (`hekma serve` and any CLI command): the
sequence lives in the one database they share. A usage update and the
breaches it fires commit together, so they take consecutive seqs, followed by
the transition a `pause` or `stop` breach causes.

**Upgrading from a file-backed outbox.** The first open of an engine with
the state-database outbox imports `outbox.jsonl` and `outbox-ack.json` once —
every retained entry keeps its seq, the ack carries over, and the next event
continues above both — then renames them `*.migrated`. A Host cursor held
across the upgrade stays valid.

## The cooperative caveat (read this before exposing it)

The API is served **from the host process itself**: every request runs on the
host's runtime, next to the supervision loop and the shutdown drain. A slow or
stalled client can hold host work the same way a slow CLI invocation can. v0
is operator tooling for a trusted operator or a local dashboard — **not** a
multi-tenant service. Beyond loopback it requires TLS, but there is still no
rate limiting and no request isolation in v0: do not expose it on a public
interface.

Two capabilities deserve to be spelled out:

- **The bearer is operator-grade trust.** The `manifest` source on
  `POST /v0/instances` points the host at an `adapter.toml` on disk and the
  host LAUNCHES the adapter it names — so a bearer holder can direct the host
  to execute any adapter-shaped binary the host process can read, on any path
  the request names. That is precisely what the API is for (it supervises
  agents), and precisely why it is loopback tooling: whoever holds the bearer
  holds the host's launch authority.
- **A local dashboard needs CORS, which v0 does not send.** Browser pages
  cannot call this API from another origin without CORS headers; driving the
  API from a web UI today means a local proxy on the same origin. CORS is an
  additive 0.x decision deferred until a real in-tree dashboard Host exists —
  not a gap you should paper over with credentials.

On shutdown the API stops accepting first, the open SSE streams end, and only
then does the agent drain run — so a client always sees the API go away before
agents stop. Two bounded-degradation facts about requests, stated plainly:
a long-window `stop` (up to 86 400 seconds) holds its HTTP connection until
the ladder finishes — and the drain RE-ISSUES stops, so even a request that
never sees its response converges on the same outcome; and a connection still
held past the shutdown's 1-second graceful window (a stalled request body, a
non-cooperative client) is ABORTED at that bound, with the abort noted on the
host's stderr — never a silent hang.

## A two-terminal smoke

```bash
# Terminal 1: the host (secret via env; or put CONTROL_BEARER in secrets.toml).
# The local endpoint and its token are in <state-dir>/control.json as well.
CONTROL_BEARER="s3cret" hekma serve --listen 127.0.0.1:8477 --bearer-secret CONTROL_BEARER

# Terminal 2: drive it
curl -s http://127.0.0.1:8477/v0/version                                     # 401
curl -s -H "Authorization: Bearer s3cret" http://127.0.0.1:8477/v0/version   # 200
# The compiled-in `mock` kind registers on every machine but has no launch
# command — to drive start/stop for real, register a manifest adapter (an
# ABSOLUTE path to its adapter.toml directory) instead of `kind`.
curl -s -H "Authorization: Bearer s3cret" -H "Content-Type: application/json" \
     -d '{"name":"worker","kind":"mock"}' http://127.0.0.1:8477/v0/instances # 201 + Location
curl -s -N -H "Authorization: Bearer s3cret" http://127.0.0.1:8477/v0/events # live SSE frames
curl -s -X POST -H "Authorization: Bearer s3cret" \
     -d '{"window_secs":30}' http://127.0.0.1:8477/v0/instances/worker/stop  # 200 (running) / 409 (registered only)
```
