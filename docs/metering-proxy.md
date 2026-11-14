---
title: "The metering proxy"
description: "One loopback proxy per state directory meters every model call, attributes it to a Run, enforces block, and answers in a versioned wire contract, proxy-wire v1."
---

# The metering proxy

> **On main, not yet released.** The shared metering proxy ships with the next release. It replaces the per-agent metering listener. Key custody is a preview, not yet supported. Scent-Check Modes, [announce](announce.md) and [hold, release, reroute and reject](#hold-release-reroute-and-reject) are on main, and so is [serving beyond loopback](#beyond-loopback), over TLS and with a credential on every call from another machine. See [Status and roadmap](https://hekma.ktesio.dev/docs/status/#roadmap).

Hekma meters model calls by sitting between a client and its model provider. The **metering proxy** is one HTTP listener on `127.0.0.1`, owned by the Service Host (`hekma serve`). Each call it forwards is attributed to a [Run](runs.md), counted once in the usage ledger, checked against that Run's budgets, and relayed back. Supervised agents reach it through the base URL Hekma injects. Any other client reaches it through an **endpoint binding** you create.

This page is the complete contract: where the proxy listens, how a call finds its Run, what is added to and removed from a request, and every response the proxy makes itself. That contract is versioned as **`proxy-wire v1`**. Every refusal body names the version, and later releases only add to it.

## Where it listens

| Question | Answer |
|---|---|
| Address | `127.0.0.1` by default. `--metering-listen <IP>:<port>` binds another address (an IP literal, never a host name); beyond loopback it requires TLS and a credential on every call from another machine. See [Beyond loopback](#beyond-loopback). It is always one listener. |
| Port | `--metering-listen <port>` if you pass it. Otherwise the port recorded for this state directory. Otherwise the OS picks one, and Hekma records it. The port is **sticky**: a restart binds the same one, so clients and adopted agents keep a live address. |
| Across an upgrade | `hekma serve --upgrade` hands the listening socket itself to the new host (Unix): it never closes, so no connection is refused while the hosts change over; a connection that arrives meanwhile waits until the new host accepts it. A plain restart (stop, then start) does refuse connections until the new host binds; clients see that as retryable. See [Run Hekma as a service](hosting.md#agents-in-their-own-shim-a-preview). |
| Finding it | The started notice (`the metering proxy is listening on http://127.0.0.1:<port>.`), `control.json`, `GET /v0/proxy` and `hekma binding show`. Beyond loopback they also name the address clients on other machines use (`https://<public host or bind address>:<port>`). |
| A taken port | Hekma never moves to another port. The host keeps serving the Control API, says which port is taken and why, and refuses `engine-observed` starts until you free the port or pass another. |
| Embedding | An embedding application gets the proxy on an ephemeral, unrecorded port. A read-only engine never runs one. |

**The Host and Origin guard.** Before anything else, the proxy checks two request headers. `Host` must be a loopback address (`127.0.0.1` or anything in `127.0.0.0/8`, `::1`, or `localhost`), with any port, or a name you list with `hekma serve --proxy-allowed-hosts <name,...>`. An `Origin` header, which browser pages send and SDKs do not, must be on the list you give with `--proxy-allowed-origins <origin,...>`; by default any `Origin` is refused. A request that fails either check gets `403 hekma_bad_host` before any upstream is dialled. The proxy never adds `Access-Control-*` headers and never answers a browser's preflight permissively. This stops a web page from reaching the proxy through DNS rebinding.

On loopback the proxy asks for no credential. A Run Key tells it *which* Run a call belongs to; it is not a gate. Another OS user on the same machine can reach a loopback port, and can bind it while no host holds it (see [Port squatting](#port-squatting)). Treat the proxy as reachable by every local user, and serve it only on machines you control.

## How a call finds its Run

The proxy decides everything about a call from one consistent view of the Runs, taken when the request arrives. It checks, in this order:

1. **A credential in the URL.** An `hk_run_`, `hk_ref_`, `hk_ann_` or `hk_pxy_` token anywhere in the path or query, raw or percent-encoded, is refused with `400 hekma_credential_in_url`. Nothing is forwarded. (The proxy refuses the token but does not revoke it, because anyone on loopback could have sent the request.)
2. **A presented Run Key**, in `x-hekma-run-key`, or an `hk_` token in `authorization: Bearer` or `x-api-key`:
   - An unknown key is refused with `401 hekma_unknown_credential`.
   - A key revoked in the last 24 hours is refused with `401 hekma_credential_revoked`, so the client is told to stop rather than to fix its key. An older revoked key answers as unknown.
   - A refresh or announce credential is refused as unknown.
   - A Run Key in the **API-key slot** (`authorization`, `x-api-key`, `x-goog-api-key` or `api-key`) asks for [key custody](run-keys.md#key-custody-preview--not-yet-supported): the proxy swaps in the provider key Hekma holds for that Run. If the Run holds no key, or the Run Key was not issued to a supervised agent or a credentialed open, the call is refused with `400 hekma_custody_unavailable`. Without custody, send your provider key in `authorization` and the Run Key in `x-hekma-run-key`.
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

An id is at most 128 visible ASCII characters, and a hint carries at most 16 labels, each checked like any label. A hint's labels apply when it creates the Run; later different labels are ignored. Each parent or binding holds at most 1,000 open hint Runs. Past that, or past 8 levels of nesting, calls stay on the parent, still attributed, and a diagnostic says so at most once a minute.

The first call for a new Run waits up to one second for the Run to be created. Past that it is refused with `503 hekma_run_unavailable` (retryable) and not forwarded.

### Announce

`x-hekma-announce: <id>` claims an identity in the same namespaces a hint uses: a child beneath a Run Key or a Run route, else `<id>` beneath the binding, labelled or not (it is never ignored, because it issues a Run Key). It wins over an `x-hekma-run` hint on the same call, and `x-hekma-labels` gives the Run its Caller labels. A call that announces is served under every Scent-Check Mode. The claim waits within the same one-second bound.

When the claim **creates** the Run, the call's response carries the new Run's credentials once, whatever status it answers:

- `x-hekma-run-key: hk_run_…` and `x-hekma-refresh: hk_ref_…`, with `cache-control: no-store`;
- never again: a later announce of the open identity renews its Lease and issues nothing.

This Run Key never draws key custody. Past a cap, the call stays on the parent or binding Run, nothing is issued, and a diagnostic says so at most once a minute. See [Announce and Leases](announce.md).

## What the proxy changes

**On the way out:**
- Every request header whose name starts with `x-hekma-` is removed.
- Hop-by-hop headers are handled as any proxy handles them.
- The outbound URL is the upstream base URL, plus the path after the prefix, plus the query. The proxy checks that its scheme, host and port still equal the upstream's before it dials, so a crafted path cannot send the call elsewhere. A `CONNECT` request, or an absolute-form target naming another host, is refused with `400 hekma_bad_target`. Redirects are relayed to the client, never followed.
- Streaming requests that ask for `"stream": true` get `stream_options.include_usage` added (unless they set `stream_options` themselves), so the final usage frame can be counted.
- Two **identity headers** are added, unless they are turned off:
  - `x-hekma-run-id`, the Run's id;
  - `x-hekma-labels`, the Run's labels as sorted, percent-encoded `key=value` items joined by commas.

  Neither ever carries a token, a token id or request content. **They are on by default, so your model provider receives each call's Run id and labels.** Turn them off if that is not what you want: `hekma proxy set identity-headers off` stores the setting and applies it to the very next call, `hekma binding set <name> --identity-headers on|off|inherit` sets one binding's own, and `hekma serve --no-identity-headers` or `--identity-headers` overrides the stored setting for one run. A binding's own setting wins, then the serve flag, then the stored setting. See [Hekma in front of a gateway](gateways.md#turning-the-identity-headers-off).

**Under key custody** (a preview; see [Run Keys and key custody](run-keys.md#key-custody-preview--not-yet-supported)):
- The call must be presented with a Run Key issued to a supervised agent or a credentialed open. Its Run, a Run above it, or its binding, must hold a provider key.
- The outbound URL must be `https`, on the origin pinned for the key; anything else is `400 hekma_bad_target`. Its path must be on the key's path list, else `400 hekma_path_not_allowed`. In both cases nothing is forwarded.
- Every credential the client sent (`authorization`, `x-api-key`, `x-goog-api-key`, `api-key`) is removed, and the held key is inserted once, in the provider's own header. The proxy asks for an uncompressed response.
- Every exact occurrence of the held key in the response's headers and body is replaced with `hekma_redacted_key`.
- The proxy reads keys from memory and picks up a changed secrets file within 30 seconds.

**On the way back:** the response is relayed as the upstream sent it. If the Run, or any Run above it, holds a latched breach, the response also carries `x-hekma-breach: <code>[, <code>]`.

Responses are buffered whole before they are relayed, so through the proxy a streaming call's first token arrives when the whole response has. Incremental relay is planned.

## Metering, once per call

- Each call gets a call id (`call_` + a ULID). Its usage is parsed from the response (the OpenAI and Anthropic JSON and streaming shapes), and recorded once per (Run, call id). A replay of the same record is a duplicate, never a second charge. A client that retries makes a new call with a new id, and is charged again because the provider charged it again.
- Before anything is forwarded, the call reserves room for its records. When none frees up within one second, the call is refused with `503 hekma_ledger_unavailable`, so nothing is ever spent that cannot be recorded.
- Records reach the store in batches. The proxy relays the response before its batch commits, and a call made in that window (about a quarter of a second) can pass a budget the previous call just crossed. That is the same bound the per-agent listener had.
- Each call's record (its Run, the presenting key's token id, the model, the outcome and the token counts, never its content) lands in the **calls store**, `<state dir>/calls/`.
- If the host is killed, calls in flight fail at the client as connection errors. At the next start, Hekma closes their records: `recovered` when the usage made it into the ledger, else `crash` with unknown tokens. A call relayed in the last batch window before a hard kill can go uncounted; it shows up as a `crash` record, never as a double count.
- A client that disconnects cancels the upstream request, and the call is recorded as `client-disconnect` with unknown tokens. The provider may still bill a partial generation Hekma cannot see.

## Reading the records

Every call the proxy admits leaves one record in the calls store. Read them with `hekma calls list` and `hekma calls show` (see [Commands](commands.md#hekma-calls-listshow)), with `GET /v0/calls` and `GET /v0/calls/{call_id}` (see [Control API](control-api.md#per-call-records)), or with `Engine::list_calls` and `Engine::call` when you embed the engine.

A record carries:

| Field | Meaning |
|---|---|
| `call_id` | `call_` + a ULID, minted when the call is admitted |
| `run_id` | The Run the call was charged to |
| `token_id` | The 12-hex label of the Run Key the call presented; `null` when it presented none |
| `control_depth` | The Run's Control Depth at the call |
| `binding` | The binding the call came through; `null` off a binding |
| `disposition` | What the proxy did: `pass`, `block`, or `enforce` (refused under the `enforce` Scent-Check Mode); `hold`, `reroute` and `reject` are reserved |
| `hold_end` | Always `null` until holds ship |
| `outcome` | `completed`, `refused`, `upstream-error`, `client-disconnect`, `shutdown`, `crash` or `recovered` |
| `status`, `status_class` | The status the client got (`null` when none) and its class: `2xx`, `3xx`, `4xx`, `5xx`, `refused` or `transport-error` |
| `model` | The model the response or request named. A value that contains anything shaped like a Hekma credential (an `hk_run_`, `hk_ref_`, `hk_ann_` or `hk_pxy_` prefix in any case, followed by credential characters) reads back as `[redacted]` |
| `input_tokens`, `output_tokens`, `cached_tokens` | The counts the provider reported; `null` when unknown, never a made-up `0` |
| `started_at`, `ended_at`, `latency_us`, `upstream_us` | When the call started and ended, the whole call's time, and the upstream leg's (`null` when nothing was dialled) |

A record never holds a prompt, a response, a header, a key or the upstream's address.

- **Order and paging:** newest call id first. Each page is at most 1,000 records (default 50), and its `next_cursor` continues the same listing with no gap and no duplicate. A record whose call closes after a page was read appears on a later page when its id is below the cursor.
- **Freshness:** a call is listed within one batch window (about a quarter of a second) after its response is relayed. A call still in flight is not listed.
- **The problems view** (`--problems`, `view=problems`) lists every call that did not pass cleanly: a disposition other than `pass`, an outcome other than `completed`, or a status class other than `2xx` or `3xx`.
- **The scan budget:** a page examines at most 100,000 records. A filter that matches almost nothing can stop before the page fills; the page then says `scan_limited: true`, names how far back it searched (`searched_back_to`), and its cursor continues the search. The unfiltered, Run-scoped and problems listings always read an index.
- **Segments:** the calls store is a set of SQLite files. A file written by a newer Hekma, or one that cannot be read (not SQLite, no calls table, or a record that will not decode), is skipped: `skipped_segments` counts it, `skipped_files` names it and says why (`hekma calls list` prints the same note), and the other files are still served. The files are merged by call id, whatever their names say, so a page never skips or repeats a record. Looking one call up by id over a file that could not be read answers `503 calls-unavailable` rather than `404`, because the record might be in it.
- **Files by day, and how long they are kept:** the proxy writes a new file each UTC day, `calls/<date>.db`; a call still in flight at midnight moves to the new file, so its record lands in one. A file past the calls retention window (seven days unless changed with `hekma run retention --calls <days>`) is deleted whole, never while it holds a call in flight.
- **Who can read them:** the Operator only. Reads take no lock the proxy or supervision uses, and work from a read-only open while a Service Host writes.

## Budgets and `block`

Usage through the proxy enforces exactly like any other usage: the Run's own budgets and every Run above it, through the same [Policies](policies.md) and [budget verbs](commands.md). A change to a budget applies on the very next call.

Under the `block` [Breach Action](governance.md), the proxy refuses a Run's calls while the Run, or any Run above it (up to eight levels), holds a latched breach. `GET`, `HEAD` and `OPTIONS` stay exempt. `block` works for every Run with a wire path, not only for supervised agents. So do `pause` and `stop`: on a Run that owns no process, a latched `pause` **holds** the Run's calls and a latched `stop` **refuses** them like `block`, both until the ceiling is raised or reset. Only a Run with no wire path still falls back to `warn` for them.

## Hold, release, reroute and reject

An Operator or a Host can act on a Run's model calls at the proxy, with `hekma hold`, `hekma release`, `hekma reroute` and `hekma reject` (or the [Control API](control-api.md#hold-release-reroute-and-reject)). Each acts on one Run, or every open Run a Selector matches. Each sets or clears only its own reason, and a reason on a Run applies to every Run beneath it. The process keeps running, and a call already forwarded completes and is billed.

| Reason | What a new call gets |
|---|---|
| **reject** (`hekma reject`), or a breach latched under `block` or `stop` | Refused with a `402`, never forwarded. A breach answers `hekma_budget_block` as before; an Operator's reject answers `hekma_run_reject`. `GET`, `HEAD` and `OPTIONS` pass. |
| **hold** (`hekma hold`), or a breach latched under `pause` | Parked until released |
| **reroute** (`hekma reroute --via <binding>/<route>`) | Forwarded to the route's upstream instead |
| none | Forwarded as usual |

When reasons combine, the strongest wins: reject, then hold, then reroute. `hekma release` clears only the hold, `hekma reroute --off` only the reroute, a raised or reset ceiling only its breach, and an Operator's reject lasts until the Run ends.

**Holding.** A held call waits at the proxy, after the `block` and Scent-Check checks and before its body is read. Calls are released in the order they arrived. Hekma sets no time limit on a hold: only the client's own timeout ends one. Each Run holds at most 32 calls (`MeteringProxyOptions.max_held_per_run`); past that, a call gets `503 hekma_hold_full`, which SDKs retry. When a hold is released, each call is checked again before it is forwarded. A call whose Run Key was revoked meanwhile, whose Run ended, whose Run now refuses unscented calls, or whose Run gained a reject reason gets that refusal instead, never a forward. If the Service Host stops gracefully, each held call gets `503 hekma_hold_interrupted`; a retried call is held again, because the hold belongs to the Run and is stored.

**Rerouting.** A route is one you stored with `hekma binding route add`. A rerouted call goes to the route's upstream with its own path after the binding prefix and its query, so set the route's base to fit the paths your client sends. A route with its own custodied key uses that key. Otherwise a Run whose provider key Hekma holds can be rerouted only to the origin its key is pinned to: a held key never leaves its origin. A reroute under a hold forwards the held calls to the route when the hold is released.

**Seeing it.** A held Run that owns no process shows `held`, and `hekma ps` shows how many calls it holds and how long the oldest has waited. Run documents carry `dispositions`, `held_calls`, `oldest_held_at` and `last_hold_end` (how its most recent held call ended without a release: `disconnect` for a dropped connection, `shutdown` for a stop). Per-call records carry the disposition (`hold`, `reroute`, `reject`) and the `hold_end` (`released`, `rerouted`, `rejected`, `disconnect`, `shutdown`).

## Scent-Check

Right after the `block` check, the proxy applies the [Scent-Check Mode](run-keys.md#scent-check-modes) to a call that no valid Run Key identified (an **unscented** call): the strictest mode of its Run and every Run above it. Under `off` it is served; under `shadow` it is served, counted, and its Run gets the `unscented` Anomaly; under `enforce` it is refused with `401 hekma_unscented_call` and nothing is forwarded. A latched `block` refuses first, and `enforce` has no exemption for `GET`, `HEAD` or `OPTIONS`.

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
- **An optional custodied-key reference** (`secret:NAME`). It's stored as the reference only. Under key custody, a call on the binding presented with a Run Key Hekma issued (to a Run beneath the binding's Run) uses it; a call with no credential never does. A binding with a custodied key needs an `https` upstream on the key's pinned origins, or creating or changing it is refused.
- **Up to 16 reroute routes**, each a name and an upstream. They're stored and checked now, and used when reroute ships.

There are at most 256 bindings. Changing a binding's prefix or upstream keeps its Runs. Changing its labels updates its open Runs. Deleting it ends its Run and its split Runs with cause `binding-removed`. Every change is one `binding` event.

Each binding can also hold one active **announce** credential (`hk_ann_`) and one active **proxy-access** credential (`hk_pxy_`):
- `hekma binding credential issue` prints the credential once and stores only its hash. Issuing again revokes the previous one.
- `revoke` works per class, and `list` shows token ids and states.
- **The announce credential** announces Runs beneath its binding on `POST /v0/runs/announce`, and heartbeats or closes the Runs it created (see [Announce and Leases](announce.md)).
- **The proxy-access credential** is what a client on another machine sends as its API key to a proxy bound beyond loopback, on its binding's prefix only (see [Beyond loopback](#beyond-loopback)). On a loopback-bound proxy it is unknown.

Bindings are managed by the Operator only: through `hekma binding` (see [Commands](commands.md)) or `/v0/bindings` (see [Control API](control-api.md)).

## Beyond loopback

By default the proxy binds `127.0.0.1`. To let clients on other machines reach it, bind a wider address with its own TLS certificate and key:

```bash
hekma serve --metering-listen 0.0.0.0:8400 \
  --proxy-tls-cert proxy-cert.pem --proxy-tls-key proxy-key.pem \
  --proxy-public-host proxy.example.com
```

- **TLS is required.** Without `--proxy-tls-cert` and `--proxy-tls-key`, a bind beyond loopback is refused at start (exit `2`), before anything opens. Both PEM files must be readable only by their owner (mode `0600` on Unix; on Windows the user profile's own access control applies), and so must the Control API's `--tls-cert`/`--tls-key`. TLS 1.2 is the minimum. Self-signed certificates work; use one your clients trust. Hekma does not provision or rotate certificates, so watch their expiry.
- **One listener, two kinds of peer.** The same port serves both. A connection from this machine (`127.0.0.0/8` or `::1`) keeps every loopback rule, and may speak plain HTTP, so a supervised agent's `http://127.0.0.1:<port>` base URL keeps working. A connection from any other address must complete a TLS handshake within 10 seconds; plain HTTP from another machine is never served.
- **Every call from another machine carries a credential**, checked before its body is read or any upstream is dialled. It is one of:
  - the binding's **proxy-access credential** (`hk_pxy_`), sent as the client's API key (`authorization: Bearer …` or `x-api-key`). It works only on its own binding's prefix (elsewhere, `403 hekma_credential_out_of_scope`). It is never forwarded: when the binding holds a custodied key, that key is sent instead, under [key custody's](run-keys.md#key-custody-preview--not-yet-supported) rules; otherwise the call is forwarded with no key, and the provider's own refusal answers. A binding served this way wants a custodied key. A proxy-access credential is not a Run's identity, so its calls are unscented under [Scent-Check](#scent-check);
  - a **Run Key** of a Run Hekma did not start, in `x-hekma-run-key` or the API-key slot. A supervised agent's Run Key is local by definition: from another machine it is unknown.
  
  A call with neither gets `401 hekma_authentication_required`, and its connection closes without its body being read. A call may carry both: the Run Key identifies the Run, and the proxy-access credential must still be valid and in scope.
- **Issue the credential** with `hekma binding credential issue <name> --class proxy-access`. It is shown once and only its hash is stored; issuing again or `revoke` stops the old one at once (`401 hekma_credential_revoked` for 24 hours, then unknown). Until one is active, the started notice says every call from other machines will be refused.
- **The Host and Origin guard** applies to a call from another machine only when you configure it: with `--proxy-allowed-hosts` or `--proxy-allowed-origins` set, an unlisted value is `403 hekma_bad_host`; with neither set, TLS and the credential carry that boundary. Calls from this machine keep the loopback rule.
- **The address you hand out.** `--proxy-public-host` names the host clients use; the started notice, `GET /v0/proxy` (`public_url`), `control.json` and `hekma binding show` show `https://<public host>:<port>`. Without it they show the bind address, which a client may not be able to dial. The bind is never stored: pin the flags in your service definition, or a restart without them binds loopback again.

### Port squatting

The proxy's port is sticky, so while no host is running, another user on the same machine could listen on it and collect what clients send. Whenever the proxy binds, the started notice says whether this machine has other user accounts that can log in, or that Hekma cannot tell. Where it does, use socket activation (systemd or launchd), run Hekma on a machine you do not share, or bind a privileged port where the operating system allows one.

## Limits and timeouts

| Limit | Value | Past it |
|---|---|---|
| A request or response body | 64 MiB | `413 hekma_body_too_large` for a request, `502 hekma_upstream_error` for a response |
| Concurrent calls | 512 | `503 hekma_proxy_busy` |
| Buffered bytes across calls | 512 MiB | `503 hekma_proxy_busy` for a request, refused as its body arrives and before anything is sent upstream. A response is never refused for it: the provider has already billed the call, so the response is recorded and relayed, and a diagnostic (at most once a minute) says the budget was exceeded |
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
| 400 | `hekma_custody_unavailable` | A Run Key sits in the API-key slot, and its Run holds no key or the key is not one custody serves | `false` |
| 400 | `hekma_bad_target` | `CONNECT`, a target naming another host, or a custodied call whose upstream is not its pinned `https` origin | `false` |
| 400 | `hekma_path_not_allowed` | A custodied call to a path outside the key's list | `false` |
| 403 | `hekma_bad_host` | The `Host` is not loopback or listed, or an `Origin` is not listed | `false` |
| 401 | `hekma_unknown_credential` | The credential is not a Run Key Hekma honours | `false` |
| 401 | `hekma_credential_revoked` | The Run Key was revoked in the last 24 hours | `false` |
| 401 | `hekma_authentication_required` | A call from another machine to a proxy bound beyond loopback carries no credential | `false` |
| 403 | `hekma_credential_out_of_scope` | A proxy-access credential on another binding's prefix, or on a Run route | `false` |
| 401 | `hekma_unscented_call` | The call carries no credential Hekma issued, and its Run's Scent-Check Mode is `enforce` | `false` |
| 402 | `hekma_budget_block` | A breach latched under `block` or `stop` holds for the Run or a Run above it | `false` |
| 402 | `hekma_run_reject` | An Operator rejected the Run's (or a Run above it) calls | `false` |
| 403 | `hekma_run_out_of_scope` | The Run Key's Run is not the route's Run or beneath it | `false` |
| 404 | `hekma_no_route` | No Run route or binding matches | `false` |
| 413 | `hekma_body_too_large` | The request body is over 64 MiB | `false` |
| 502 | `hekma_upstream_error` | The upstream could not be reached, failed TLS, reset, or sent too large a body | not sent (the SDK's own default) |
| 503 | `hekma_proxy_busy` | The proxy is at its concurrency or buffer limit | `true` |
| 503 | `hekma_ledger_unavailable` | The call could not be recorded, so it was not sent | `true` |
| 503 | `hekma_run_unavailable` | Its new Run was not created in time | `true` |
| 503 | `hekma_hold_full` | The Run's calls are held and its hold queue is full | `true` |
| 503 | `hekma_hold_interrupted` | The call was held when the proxy stopped | `true` |

The `402` body and headers are unchanged from earlier releases; see [Governance](governance.md).

### Headers

| Header | Direction | Meaning |
|---|---|---|
| `x-hekma-run-key` | Request | The Run Key naming the call's Run |
| `x-hekma-run` | Request | A hint: a child or split Run's id |
| `x-hekma-labels` | Request | The hint's or announce's Caller labels |
| `x-hekma-announce` | Request | Claim this identity (see [Announce](#announce)) |
| `x-hekma-*` | Request | Every one is removed before forwarding |
| `x-hekma-run-id` | Outbound | The Run's id (unless the identity headers are off) |
| `x-hekma-labels` | Outbound | The Run's labels (unless the identity headers are off) |
| `x-hekma-breach` | Response | The latched breaches of the Run or a Run above it |
| `x-hekma-run-key` | Response | A first announce's new Run Key, once |
| `x-hekma-refresh` | Response | A first announce's new refresh credential, once |

## Moving from the per-agent listener

Earlier releases gave each `engine-observed` agent its own listener on its own port. Now:
- **Manifests don't change.** `[config."metering.base_url"] env = "OPENAI_BASE_URL"` still maps the injected value, and `metering.upstream_base_url` still sets the upstream.
- **The injected value now carries a path:** `<proxy url>/_hekma/runs/<run id>` instead of `http://127.0.0.1:<port>`. A client that appends its paths to the base URL works unchanged. One that keeps only the base URL's host and drops its path gets `404 hekma_no_route`, loudly, never a wrong attribution.
- **`--metering-listen`** keeps its name and now sets the shared proxy's port.
- **Agents running across the upgrade:**
  - With an unpinned old host, their listeners died with it, as before. Restart them.
  - With a pinned port, one agent could have been listening on it. When the new host adopts exactly one such agent, its calls (which carry no Run route) reach its own Run through a **legacy root route** until that Run ends. `hekma ps` notes it, and creating a `/` binding meanwhile is refused.
  - When more than one such agent is adopted, none gets the route, and a diagnostic names them. Restart each one.
- **An adopted agent's base URL stays live** across host restarts, because the port is sticky. A detached start of an `engine-observed` instance is still refused (exit 5), because the proxy lives inside the host.

## What is still planned

- **Key custody, supported:** it is a preview today, until the pending security rulings are recorded and its test criteria pass in CI.
- **Helpers for CrewAI and LangGraph.**
- **Incremental streaming relay.**
- **Mutual TLS, and provisioning or rotating the proxy's certificate.**

See [Status and roadmap](https://hekma.ktesio.dev/docs/status/#roadmap).
