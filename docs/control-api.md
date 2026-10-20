---
title: Control API (v0)
description: The HTTP/JSON surface served from `hekma serve` — routes, bearer auth, the SSE and cursor-pull event contracts, and the v0 stability marker.
---

# Control API (v0)

The Service Host can expose a local HTTP/JSON surface over the agents it
supervises: registration, lifecycle, configuration, budgets and usage (through
the Fleet row), and live events. It exists **only** when you configure it, it
requires a bearer token on **every** route, and it is versioned `/v0` and
marked **unstable** — the surface freezes only after a second hosting platform
validates it, so fields and error wording are additive-only until then and may
still change.

## Turning it on

```bash
CONTROL_BEARER="s3cret" hekma serve --listen 127.0.0.1:8477 --bearer-secret CONTROL_BEARER
```

- `--listen <addr>` — the address to bind (a socket address like
  `127.0.0.1:8477`). Default: off — with no flag there is no API and `serve`
  behaves exactly as before.
- `--bearer-secret <name>` — the NAME of the bearer secret guarding the API.

The pair is validated together: **both or neither** — a half-configured API is
a usage error before anything starts, and so is an unparseable address. The
secret NAME resolves at startup through the same composite resolver every
Hekma secret uses: the environment variable with that exact name first, then a
key in the state root's `secrets.toml` (which must not be group/other
readable). Two resolved shapes refuse the startup with a typed error before
anything binds or opens: an **unresolvable** name (the API never serves
unauthenticated) and a secret that resolved to an **empty value** (an empty
bearer could never authenticate — the error names the reference to fix). When
the API is up, the host prints a
`control api listening on <addr> (v0, unstable — bearer required)` notice on
stderr; that notice is the address of record (with port `0`, it carries the
OS-chosen port). A **non-loopback** bind works (container recipes legitimately
bind `0.0.0.0`) but warns loudly: this surface has no TLS and bearer-only
auth — do not expose it on an untrusted network. For the full container
posture — the static build, the stop-grace mapping, the volume-backed state —
see [Running Hekma as a container supervisor](hosting.md).

## Auth

Every route — `/v0/version` included — requires
`Authorization: Bearer <token>`. The check is a constant-time comparison; a
missing or wrong token answers `401` with a `WWW-Authenticate: Bearer`
challenge and a static refusal body that never echoes the presented token. No
handler runs for an unauthenticated request, and no secret VALUE ever enters a
response, a log line, or an SSE frame — the API projects no secret reveal at
all (a `secret:NAME` config value renders masked, exactly as the CLI displays
it).

## Routes

| Method | Path | Purpose | Success |
|--------|------|---------|---------|
| `GET` | `/v0/version` | Self-description: identity, `v0`/`unstable` marker, and the engine schema versions this build projects | 200 |
| `GET` | `/v0/fleet` | Every instance's Fleet row (the `list --json` document, verbatim) | 200 |
| `POST` | `/v0/instances` | Register from exactly one adapter source: `{"name","kind"}` or `{"name","manifest"}` (an ABSOLUTE `adapter.toml` directory path) — both or neither is a 400 | 201 + `Location: /v0/instances/{name}` + read-back |
| `GET` | `/v0/instances/{name}` | One instance's Fleet row (the `show --json` document) | 200 |
| `DELETE` | `/v0/instances/{name}` | Remove a STOPPED instance (a running one is a 409; stop it first) | 204, empty |
| `GET` | `/v0/instances/{name}/status` | Runtime status: the registry row, effective restart policy, restart count, failure cause | 200 |
| `GET` | `/v0/instances/{name}/capabilities` | The effective Capability Declaration projected onto this OS | 200 |
| `GET` | `/v0/instances/{name}/config` | The effective config (the `config get --json` document; values masked) | 200 |
| `PUT` | `/v0/instances/{name}/config` | Set one key: `{"key","value"}` — acknowledged with the post-write read-back plus the engine's warn-only steering warnings | 200 |
| `GET` | `/v0/instances/{name}/events` | Cursor pull: the engine's resync batch for one instance (lossless + idempotent) | 200 |
| `POST` | `/v0/instances/{name}/start` | Start the agent | 200 + read-back |
| `POST` | `/v0/instances/{name}/stop` | Stop through the bounded ladder; optional body `{"window_secs": N}` (1..=86400, out-of-bounds is a 400). A PRESENT body must parse — only a truly ABSENT body means the engine's default window | 200 + read-back |
| `POST` | `/v0/instances/{name}/pause` | Pause the agent | 200 + read-back |
| `POST` | `/v0/instances/{name}/resume` | Resume a paused agent | 200 + read-back |
| `GET` | `/v0/events` | The live SSE stream (below) | 200, stays open |
| `GET` | `/v0/export` | The global outbox pull (below): ONE bounded batch of committed events in ONE global order, ack-cursor-resumable | 200 |
| `POST` | `/v0/export/ack` | Advance the durable export ack cursor: `{"seq": N}` — monotonic and idempotent, the cursor as persisted comes back | 200 |

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
    "log": 1
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
| `invalid_body` | 400 | A body/query failed validation: malformed JSON, `kind`+`manifest` together, `window_secs` out of bounds, malformed cursor, a RELATIVE `manifest` path |
| `invalid_body` | 415 / 422 | A body the extractors reject: no usable content-type (415), or JSON that does not fit the route's type (422) |
| `invalid_name` | 400 | The instance name is not a legal name |
| `not_found` | 404 | The instance does not exist — or the path matches no route |
| `method_not_allowed` | 405 | The path exists but the method does not |
| `conflict` | 409 | The action contradicts the instance's state (stop on a stopped instance; remove a running one) |
| `cursor_invalid` | 409 | The cursor contradicts the journal: the per-instance pull's cursor PAST the family's committed count, an export cursor below the outbox journal's retained floor or PAST its committed tail, or an export ack past the tail — the engine's truncation/retention guards, surfaced verbatim (never a clamp) |
| `internal` | 500 | Something failed on the host side; the message names it |

## Events

Two complementary surfaces, composed the engine's documented way — **subscribe
first** (the SSE stream), then act, then pull the durable record.

**Live — `GET /v0/events` (SSE).** One engine event bus subscription per
connected client; each event rides one `data:` frame whose payload is the
engine's own kind-tagged JSON (`{"kind":"transition",…}`,
`{"kind":"budget_breach",…}`, `{"kind":"usage_update",…}`) — never re-shaped.
A client that falls behind the bus gets an in-stream
`{"kind":"lagged","skipped":N}` frame naming the gap and the stream continues
at the current tail (the skipped events stay readable in the durable logs —
the cursor pull is the lossless heal; lag is surfaced, never silent). The
stream ends when the engine is gone **or the API shuts down** — a client sees
the connection close and can reconnect; the engine is unaffected. (The end
fires even if the shutdown already fired before the stream's first poll — a
late subscriber is never left hanging.)

**Durable — `GET /v0/instances/{name}/events?cursor=…`.** The engine's
`resync` batch for one instance, verbatim: family-major, cursor-lossless, with
`torn_tail_skipped` surfaced in the payload when a torn tail was skipped. The
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
engine's global outbox: one JSON-Lines journal (`outbox.jsonl`) at the state
root, written at the same three commit points the event bus publishes from
(after the durable family commit, before the publish — the journal never
contains an event that did not commit, and journal order == bus order ==
durable-log order). Every record carries a `seq` that only ever increases,
across restarts too.

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
- **`retained_from`** is the seq of the journal's first RETAINED entry. The
  journal is bounded (compacted at append time): the acked prefix is
  dropped first, and only if the cap is still crossed the oldest UN-ACKED
  entries too — never the very last entry (a compaction never empties the
  journal, so a crash before the next append cannot restart the seq
  sequence), and every un-acked eviction is announced on the host's stderr.
  A Host that pulls regularly never loses an un-acked event; a Host that
  goes silent past the cap does, honestly.
- **`torn_tail_skipped`** mirrors the per-instance pull: the journal's
  trailing line was skipped as a torn (crash-cut) append, surfaced — never
  silent. The next append repairs the tear.
- **`blank_lines_skipped`** counts interior blank/whitespace journal lines
  this read passed over. Zero on a healthy journal; nonzero is a surfaced
  partial-corruption hint (an external edit touched the file) — counted,
  never silently dropped, and never a hard error while the records around
  the blanks parse.

**Refusals — `409 cursor_invalid`, never a clamp.** A cursor below the
retained floor (its entries were rotated away — serving it would silently
skip them; the lossless rule: anything at `retained_from - 1` or above is
servable, because its window starts at the first retained entry) and a
cursor past the committed tail (the journal never committed that far) are
both typed 409s whose message names both counts. Reset to
`{"seq":0}` deliberately if losing the rotated-away prefix is intended —
the engine never guesses.

**Corruption is loud, with a named procedure.** An unparseable
newline-terminated line, a valid-JSON wrong-shape record, or a
duplicate/decreasing `seq` between consecutive entries is a typed error —
never a silent skip or a twin delivery. To recover: stop the engine (or
host), quarantine the journal by RENAMING the file (a read failure at open
refuses to start the engine — the seq sequence is never minted from an
unverified counter — and re-derivation handles the file's absence), then
re-sync the families with the per-instance `events` pull, which remains the
complete heal.

**Cross-process discipline.** The host lock excludes other HOSTS, not
command-scoped engines: running `kt agent ...` against the same state
directory while a host serves it gives the command's engine its own seq
counter on the SAME journal. Within one process, seq order == commit order;
across two processes, seqs can interleave and — if a command-scoped engine
restarts — even reuse a range. The reader REFUSES that loudly (a duplicate
or decreasing seq is the typed corruption error above) instead of
corrupting ack math or delivering a twin. Do not drive one state
directory's outbox from two engine processes; that is the one discipline
this surface cannot relax.

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
durable (`outbox-ack.json`, atomic write at the state root): it survives a
host restart, and the default pull resumes from it.

**Ordering honesty, stated plainly.** Within ONE host process, seq order ==
commit order (every append holds the supervision lock). Two engines on one
state directory (the host lock only guards `hekma serve`) each hold their
own seq counter and can interleave — the journal's physical order is then
the only honest global order there is, and that is what a reader serves.

## The cooperative caveat (read this before exposing it)

The API is served **from the host process itself**: every request runs on the
host's runtime, next to the supervision loop and the shutdown drain. A slow or
stalled client can hold host work the same way a slow CLI invocation can. v0
is loopback tooling for a trusted operator or a local dashboard — **not** a
multi-tenant service. Do not expose it on a public interface; there is no TLS,
no rate limiting, and no request isolation in v0.

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
# Terminal 1: the host (secret via env; or put CONTROL_BEARER in secrets.toml)
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
