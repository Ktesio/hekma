---
title: "The metering proxy"
description: "One loopback proxy per state directory meters every model call, attributes it to a Run, enforces block, and answers in a versioned wire contract, proxy-wire v1."
---

# The metering proxy

> **On main, not yet released.** The shared metering proxy ships with the next release. It replaces the per-agent metering listener. Key custody, Scent-Check Modes, announce, wire hold and reroute, and serving beyond loopback are still planned; see [Status and roadmap](https://hekma.ktesio.dev/docs/status/#roadmap).

Hekma meters model calls by sitting between a client and its model provider. The **metering proxy** is one HTTP listener on `127.0.0.1`, owned by the Service Host (`hekma serve`). Each call it forwards is attributed to a [Run](runs.md), counted once in the usage ledger, checked against that Run's budgets, and relayed back. Supervised agents reach it through the base URL Hekma injects. Any other client reaches it through an **endpoint binding** you create.

This page is the complete contract: where the proxy listens, how a call finds its Run, what is added to and removed from a request, and every response the proxy makes itself. That contract is versioned as **`proxy-wire v1`**. Every refusal body names the version, and later releases only add to it.

## Where it listens

| Question | Answer |
|---|---|
| Address | `127.0.0.1` only. There is no flag to widen it. |
| Port | `--metering-listen <port>` if you pass it. Otherwise the port recorded for this state directory. Otherwise the OS picks one, and Hekma records it. The port is **sticky**: a restart binds the same one, so clients and adopted agents keep a live address. |
| Finding it | The started notice (`the metering proxy is listening on http://127.0.0.1:<port>.`), `control.json`, `GET /v0/proxy` and `hekma binding show`. |
| A taken port | Hekma never moves to another port. The host keeps serving the Control API, says which port is taken and why, and refuses `engine-observed` starts until you free the port or pass another. |
| Embedding | An embedding application gets the proxy on an ephemeral, unrecorded port. A read-only engine never runs one. |

On loopback the proxy asks for no credential. A Run Key tells it *which* Run a call belongs to; it is not a gate. Another OS user on the same machine can reach a loopback port, and can bind it while no host holds it. Treat the proxy as reachable by every local user, and serve it only on machines you control.

## How a call finds its Run

The proxy decides everything about a call from one consistent view of the Runs, taken when the request arrives. It checks, in this order:

1. **A credential in the URL.** An `hk_run_`, `hk_ref_`, `hk_ann_` or `hk_pxy_` token anywhere in the path or query, raw or percent-encoded, is refused with `400 hekma_credential_in_url`. Nothing is forwarded. (The proxy refuses the token but does not revoke it, because anyone on loopback could have sent the request.)
2. **A presented Run Key**, in `x-hekma-run-key`, or an `hk_` token in `authorization: Bearer` or `x-api-key`:
   - An unknown key is refused with `401 hekma_unknown_credential`.
   - A key revoked in the last 24 hours is refused with `401 hekma_credential_revoked`, so the client is told to stop rather than to fix its key. An older revoked key answers as unknown.
   - A refresh or announce credential is refused as unknown.
   - A Run Key in the **API-key slot** (`authorization` or `x-api-key`) names the Run, but forwarding it would send it to the provider. Until key custody ships the call is refused with `400 hekma_custody_unavailable`: send your provider key in `authorization` and the Run Key in `x-hekma-run-key`.
3. **A Run route**, a path that starts with `/_hekma/runs/<run id>/`. This is the base URL Hekma injects into a supervised agent. It names the Run and that agent's upstream, and the proxy strips the prefix before forwarding. A Run route naming no open Run is `404 hekma_no_route`.
4. **An endpoint binding:** the longest binding prefix that matches on a segment boundary. `/openai` matches `/openai/v1/chat/completions`, never `/openai2`. A `/` binding catches everything else.
5. Nothing matched: `404 hekma_no_route`.

The **Run** a call is charged to is then:
- the Run Key's Run, if a key was presented. On a Run route, that Run must be the route's Run or beneath it, else the call gets `403 hekma_run_out_of_scope`. This is how a helper inside an agent charges its own child Run while using its parent's base URL.
- else the Run route's Run;
- else the binding's own Run, if the binding has labels;
- else the **unattributed** Run, which collects every call nothing else claims.

The unattributed Run is listed, metered and budgetable like any other Run.

The **upstream** always comes from the Run route or the binding. A Run Key never chooses where a call goes.

### Hints: child Runs and split Runs

Two request headers refine the Run. Neither is ever an identity on its own.

| Beneath | `x-hekma-run: <id>` does | Labels |
|---|---|---|
| A Run Key or a Run route | Names a **child Run** of that Run, created on its first call. | `x-hekma-labels` sets the child's Caller labels when it is created. |
| A binding with labels | **Splits** the binding: each id gets its own top-level Run carrying the binding's labels, so a per-run budget caps each split Run, not their sum. | The same. |
| The unattributed Run | Nothing. The hint is ignored and counted, so nobody on loopback can mint Runs without your setup. | Ignored. |

An id is at most 128 visible ASCII characters, and a hint carries at most 16 labels, each checked like any label. A hint's labels apply when it creates the Run; later different labels are ignored. Each parent or binding holds at most 1,000 open hint Runs. Past that, calls stay on the parent, still attributed, with a diagnostic.

The first call for a new Run waits up to one second for the Run to be created. Past that it is refused with `503 hekma_run_unavailable` (retryable) and not forwarded.

## What the proxy changes

**On the way out:**
- Every request header whose name starts with `x-hekma-` is removed.
- Hop-by-hop headers are handled as any proxy handles them.
- The outbound URL is the upstream base URL, plus the path after the prefix, plus the query. The proxy checks that its scheme, host and port still equal the upstream's before it dials, so a crafted path cannot send the call elsewhere. A `CONNECT` request, or an absolute-form target naming another host, is refused with `400 hekma_bad_target`. Redirects are relayed to the client, never followed.
- Streaming requests that ask for `"stream": true` get `stream_options.include_usage` added (unless they set `stream_options` themselves), so the final usage frame can be counted.
- Two **identity headers** are added, unless you start the host with `--no-identity-headers`:
  - `x-hekma-run-id`, the Run's id;
  - `x-hekma-labels`, the Run's labels as sorted, percent-encoded `key=value` items joined by commas.

  Neither ever carries a token, a token id or request content. **They are on by default, so your model provider receives each call's Run id and labels.** Turn them off if that is not what you want.

**On the way back:** the response is relayed as the upstream sent it. If the Run, or any Run above it, holds a latched breach, the response also carries `x-hekma-breach: <code>[, <code>]`.

Responses are buffered whole before they are relayed, so through the proxy a streaming call's first token arrives when the whole response has. Incremental relay is planned.

## Metering, once per call

- Each call gets a call id (`call_` + a ULID). Its usage is parsed from the response (the OpenAI and Anthropic JSON and streaming shapes), and recorded once per (Run, call id). A replay of the same record is a duplicate, never a second charge. A client that retries makes a new call with a new id, and is charged again because the provider charged it again.
- Before anything is forwarded, the call reserves room for its records. When none frees up within one second, the call is refused with `503 hekma_ledger_unavailable`, so nothing is ever spent that cannot be recorded.
- Records reach the store in batches. The proxy relays the response before its batch commits, and a call made in that window (about a quarter of a second) can pass a budget the previous call just crossed. That is the same bound the per-agent listener had.
- Each call's record (its Run, the presenting key's token id, the model, the outcome and the token counts, never its content) lands in the **calls store**, `<state dir>/calls/`.
- If the host is killed, calls in flight fail at the client as connection errors. At the next start, Hekma closes their records: `recovered` when the usage made it into the ledger, else `crash` with unknown tokens. A call relayed in the last batch window before a hard kill can go uncounted; it shows up as a `crash` record, never as a double count.
- A client that disconnects cancels the upstream request, and the call is recorded as `client-disconnect` with unknown tokens. The provider may still bill a partial generation Hekma cannot see.

## Budgets and `block`

Usage through the proxy enforces exactly like any other usage: the Run's own budgets and every Run above it, through the same [Policies](policies.md) and [budget verbs](commands.md). A change to a budget applies on the very next call.

Under the `block` [Breach Action](governance.md), the proxy refuses a Run's calls while the Run, or any Run above it (up to eight levels), holds a latched breach. `GET`, `HEAD` and `OPTIONS` stay exempt. `block` now works for every Run with a wire path, not only for supervised agents. `pause` and `stop` still need a process to act on: for a Run without one they record the breach and fall back to `warn`.

## Endpoint bindings

A binding tells the proxy that calls on a path prefix forward to an upstream, and are charged to Runs that carry the binding's labels.

```sh
hekma binding create openai --prefix /openai --upstream https://api.openai.com/v1 --label team=core
hekma binding show openai
# openai: /openai -> https://api.openai.com/v1 [team=core]  client base URL http://127.0.0.1:8400/openai
```

Point the client's base URL at the client base URL that `show` prints. A binding has:
- **A name**, using the agent-name characters.
- **A prefix:** `/` plus segments of letters, digits and `._~-`, at most 128 characters, unique, and never under `/_hekma`.
- **An upstream base URL:** `http` or `https`, with no user info, query or fragment.
- **Labels.**
- **An optional custodied-key reference** (`secret:NAME`). It's stored as the reference only, and is inert until key custody ships.
- **Up to 16 reroute routes**, each a name and an upstream. They're stored and checked now, and used when reroute ships.

There are at most 256 bindings. Changing a binding's prefix or upstream keeps its Runs. Changing its labels updates its open Runs. Deleting it ends its Run and its split Runs with cause `binding-removed`. Every change is one `binding` event.

Each binding can also hold one active **announce** credential (`hk_ann_`) and one active **proxy-access** credential (`hk_pxy_`):
- `hekma binding credential issue` prints the credential once and stores only its hash. Issuing again revokes the previous one.
- `revoke` works per class, and `list` shows token ids and states.
- **Nothing accepts either credential yet.** Announce and the proxy beyond loopback will.

Bindings are managed by the Operator only: through `hekma binding` (see [Commands](commands.md)) or `/v0/bindings` (see [Control API](control-api.md)).

## Limits and timeouts

| Limit | Value | Past it |
|---|---|---|
| A request or response body | 64 MiB | `413 hekma_body_too_large` for a request, `502 hekma_upstream_error` for a response |
| Concurrent calls | 512 | `503 hekma_proxy_busy` |
| Buffered bytes across calls | 512 MiB | `503 hekma_proxy_busy` |
| Reading request headers | 30 seconds | The connection closes |
| Connecting upstream | 10 seconds | `502 hekma_upstream_error` |
| A whole call | No limit | The client's own timeout ends a long completion |

## `proxy-wire v1` responses

Every response the proxy makes itself has `content-type: application/json` and this body:

```json
{"type":"error","error":{"type":"hekma_no_route","code":"no_route","message":"No binding matches this path. Create one with hekma binding create.","param":null,"proxy_wire":"v1"}}
```

No body ever carries a limit, a total, an agent name, a token, a token id, or any request or response content. `x-should-retry` tells SDKs whether to retry; when it says `true`, `retry-after: 1` comes with it.

| Status | `error.type` | When | `x-should-retry` |
|---|---|---|---|
| 400 | `hekma_credential_in_url` | A credential appears in the URL | `false` |
| 400 | `hekma_custody_unavailable` | A Run Key sits in the API-key slot | `false` |
| 400 | `hekma_bad_target` | `CONNECT`, or a target naming another host | `false` |
| 401 | `hekma_unknown_credential` | The credential is not a Run Key Hekma honours | `false` |
| 401 | `hekma_credential_revoked` | The Run Key was revoked in the last 24 hours | `false` |
| 402 | `hekma_budget_block` | `block` holds for the Run or a Run above it | `false` |
| 403 | `hekma_run_out_of_scope` | The Run Key's Run is not the route's Run or beneath it | `false` |
| 404 | `hekma_no_route` | No Run route or binding matches | `false` |
| 413 | `hekma_body_too_large` | The request body is over 64 MiB | `false` |
| 502 | `hekma_upstream_error` | The upstream could not be reached, failed TLS, reset, or sent too large a body | not sent (the SDK's own default) |
| 503 | `hekma_proxy_busy` | The proxy is at its concurrency or buffer limit | `true` |
| 503 | `hekma_ledger_unavailable` | The call could not be recorded, so it was not sent | `true` |
| 503 | `hekma_run_unavailable` | Its new Run was not created in time | `true` |

The `402` body and headers are unchanged from earlier releases; see [Governance](governance.md).

### Headers

| Header | Direction | Meaning |
|---|---|---|
| `x-hekma-run-key` | Request | The Run Key naming the call's Run |
| `x-hekma-run` | Request | A hint: a child or split Run's id |
| `x-hekma-labels` | Request | The hint's Caller labels |
| `x-hekma-*` | Request | Every one is removed before forwarding |
| `x-hekma-run-id` | Outbound | The Run's id (unless `--no-identity-headers`) |
| `x-hekma-labels` | Outbound | The Run's labels (unless `--no-identity-headers`) |
| `x-hekma-breach` | Response | The latched breaches of the Run or a Run above it |

## Moving from the per-agent listener

Earlier releases gave each `engine-observed` agent its own listener on its own port. Now:
- **Manifests don't change.** `[config."metering.base_url"] env = "OPENAI_BASE_URL"` still maps the injected value, and `metering.upstream_base_url` still sets the upstream.
- **The injected value now carries a path:** `<proxy url>/_hekma/runs/<run id>` instead of `http://127.0.0.1:<port>`. A client that appends its paths to the base URL works unchanged. One that keeps only the base URL's host and drops its path gets `404 hekma_no_route`, loudly, never a wrong attribution.
- **`--metering-listen`** keeps its name and now sets the shared proxy's port.
- **Agents running across the upgrade:**
  - With an unpinned old host, their listeners died with it, as before. Restart them.
  - With a pinned port, one agent could have been listening on it. When the new host adopts exactly one such agent, its calls (which carry no Run route) reach its own Run through a **legacy root route** until that Run ends. `hekma ps` notes it, and creating a `/` binding meanwhile is refused.
  - When more than one such agent is adopted, none gets the route, and a diagnostic names them. Restart each one.
- **An adopted agent's base URL stays live** across host restarts, because the port is sticky. `--detach` no longer strands an `engine-observed` agent.

## What is still planned

- **Key custody:** the Run Key in the API-key slot, the real provider key held by Hekma.
- **Scent-Check Modes:** refusing unidentified traffic.
- **Announce:** self-identifying clients and Leases.
- **Wire hold, release, reroute and reject.**
- **Incremental streaming relay.**
- **An authenticated proxy beyond loopback.**

See [Status and roadmap](https://hekma.ktesio.dev/docs/status/#roadmap).
