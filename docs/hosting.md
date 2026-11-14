---
title: Running Hekma as a container supervisor
description: Run the Hekma Service Host in a container — the static musl build, the host vs PID 1, the drain-vs-stop-grace mapping, a volume-backed state directory, and the honest limits of in-container metering.
---

# Running Hekma as a container supervisor

Every Hekma release ships a **static Linux build for x86_64** — an archive
whose binaries are linked against nothing but the Linux kernel — precisely so
the long-running Service Host can run inside a minimal container (`scratch`,
distroless, or a busybox-based image) with no glibc, no shell, and no system CA
store. The scope, stated up front: the static build is **x86_64-only today** —
there is no ARM (aarch64) Linux build yet — so a Graviton instance or an
Apple-Silicon Docker host has no native archive to pull at this time. This page
is the container recipe: where the host sits relative to PID 1, how its
graceful drain meets the runtime's stop grace, where the state directory goes,
and what a container does — and does not — add to metering enforcement.

The contracts stay in [the command reference](commands.md) and [the Control API
reference](control-api.md). This page points at them; it does not replace them.

## The static archive

Beside the per-platform archives, each release publishes
`hekma-<tag>-x86_64-unknown-linux-musl.tar.gz` with a `.sha256` beside it.
Inside are the three container-relevant binaries — `hekma` and `hkm` (the CLI
and its alias) plus `hekma-host`, the standalone Service Host binary — with
README and LICENSE. The release build verifies the static claim itself: every
packaged binary must file as static — the gate accepts both wordings `file`
prints (`statically linked`, and `static-pie linked` for rustc's musl default
static-PIE output), and only a dynamically linked binary matches neither — or
the build fails and the artifact never ships. Because the TLS stack is vendored
(rustls + ring with the bundled webpki-roots), the binaries need no
`/etc/ssl/certs` and no system trust store either.

The [installer](installation.md) keeps installing the glibc build — the static
archive is a manual download from the release page, aimed at image builds, not
at `curl … | sh` installs.

### Scratch fits the static binaries — and only some agents

The `FROM scratch` recipe at the bottom of this page works because `hekma`,
`hkm`, and `hekma-host` are static. **Your agents are separate binaries**, and
scratch can run only statically-shipped ones:

- A statically-shipped agent (a Go or Rust binary, a musl build) runs on
  scratch — `COPY` it in beside the Hekma binaries.
- A dynamic agent — `node`, `python`, or anything glibc-linked, which is the
  common case — **cannot run on scratch**. Give the final stage a base that
  carries the agent's runtime (a distroless runtime image, or an alpine variant
  with the runtime installed) and `COPY` the three Hekma binaries in the same
  way; every other part of the recipe stands unchanged.
- DNS, stated plainly: scratch bakes in no `/etc/resolv.conf` and no
  `/etc/hosts`, and musl resolves hostnames through those files. Docker and
  Kubernetes normally mount both into the container for you; if your runtime
  does not (or you override them), hostname resolution fails with no obvious
  error — copy the two files in from the fetch stage, or configure the
  metering upstream as an IP literal so nothing needs resolving. (The injected
  `base_url` is always an IP literal — the loopback proxy — so the
  agent→meter hop never needs DNS; the host→upstream hop does.)

## Where the host sits: container command, or init-supervised service

The natural container posture is the simplest one: **`hekma-host` is the
container command** (`ENTRYPOINT ["/hekma-host"]`), which makes it PID 1. That
works without an init because the host owns its shutdown signals itself: it
arms its SIGTERM and SIGINT streams *before* it opens the engine, so a TERM
arriving while agents are still being adopted is captured and held — never
delivered to the default disposition, never lost — and the drain runs once
startup settles.

Use an **init-supervised** shape (`docker run --init`, tini, or a service
supervisor as PID 1) only when something else must be PID 1: an entrypoint
script doing pre-flight work, a sidecar manager. The one requirement is that
the init **forwards SIGTERM** to `hekma-host` — an init that eats the stop
signal turns a graceful drain into a SIGKILL. Docker's built-in init and tini
both forward by default; a hand-rolled wrapper must `exec` the host or forward
explicitly.

Signals are only half of the PID-1 job — there is also **orphan reaping**.
When a process dies, its children are re-parented to PID 1; as the container
command, the host inherits an agent's stray grandchildren, and it does not run
a general reaping loop for processes it never spawned (it supervises its own
agent handles, not the whole process table). If your agents spawn subprocesses
of their own, run an init anyway — `--init` or tini reaps those zombies and
forwards TERM, and the host loses nothing: its own signal handling is
independent of who sits at PID 1.

Operator-relevant behavior is the same in either shape: while serving, stdout
stays silent and every notice is a `[hekma] `-prefixed line on stderr; a
lock refusal or a usage error exits `2`; a startup failure exits `1` — so a
restart policy can key on exit classes honestly. One exit class needs its own
honesty note: **exit `0` does not mean every agent drained cleanly.** The host
exits `0` when the drain ran, even where an individual stop failed (`NOT
stopped … — it may still be running` is a real per-instance outcome), so a
container `on-failure` restart policy will never see a degraded drain. The
truth lives in the drain output: the per-instance outcome lines on stderr
(`docker logs`), each naming who ended how — a Host that acts on drain results
should read those, not the exit code.

## The drain meets the stop grace

Set `STOPSIGNAL SIGTERM` (it is the common default; stating it keeps a runtime
with a different default from surprising you). On `docker stop` — or a Compose
`stop_grace_period`, or Kubernetes' `terminationGracePeriodSeconds` — the
runtime sends SIGTERM, waits out the grace, then SIGKILLs the container. Map
the two numbers like this:

- `--stop-window <secs>` is Hekma's own graceful window **per agent**: every
  running agent gets the whole window to exit on before the stop ladder
  escalates to a forced kill. Default `30`, bounded `1..=86400`.
- The runtime's grace must **exceed** `--stop-window` plus headroom: signal
  delivery, whatever the agents need beyond the window, and the ladder's own
  escalation once the window elapses. The drain itself is capped at **two
  passes** (the initial plan, plus exactly one re-enumeration for agents that
  entered mid-drain), and the engine's stop ladders run one at a time — so a
  many-agent fleet's worst case approaches two passes over the serialized
  per-agent windows (≈ 2×Σ window), not one window total. A
  one-agent-per-container fleet — the shape this page targets — sees about one
  window plus the escalation, which is the scope the 90-second example below is
  sized for; scale the grace with the fleet if you host more per container.

Be honest about the boundary: **the drain is bounded**. Hekma never escalates
before its window elapses, and it does not extend the runtime's grace either —
when the grace runs out, the platform SIGKILLs the container, and that kill is
the platform's call, not Hekma's. Sizing the grace past the window is what
makes SIGKILL the exception instead of the plan.

A second TERM mid-drain does not speed anything up: the host swallows repeats
with a one-line note ("a further SIGTERM arrived during the drain; swallowed —
one drain is already running and will complete"). Patience wins; repeated TERM
does not.

## The state directory on a volume

Point `--state-dir` (or the `HEKMA_STATE_DIR` environment variable) at a
mounted volume. Everything durable lives there: the single-writer lock
(`host.lock`), the secrets file, the state database, the per-agent log
families (whose event logs are human-readable mirrors of the database), and the
export outbox inside the database. Nothing of
consequence stays behind on the platform default.

Use a **local or block-backed volume**. The single-writer lock is a kernel
file lock and the state database is SQLite; network volumes (NFS, EFS, and
kin) do not reliably honor the locking and fsync semantics both depend on.
The state directory belongs on storage the node's kernel owns — a bind mount,
a local volume, or a network-attached *block device* — not on a network file
share.

One host per state directory: a second `serve` against the same volume is
refused (exit code `2`) naming the live holder's PID, and the refusal never
advises deleting the lock. That holds across containers too — a replacement
container started while the first still runs is refused the same way, because
the lock is a kernel file lock on the volume, not a per-container detail.

A replacement container that re-mounts the volume after a crash **resumes**:
adoption re-acquires the live agents, a stale lock re-acquires cleanly (the
file's contents are never trusted to mean a live holder), and the export ack
cursor is included — `GET /v0/export` replays from the durable cursor and
`POST /v0/export/ack` commits the new one.

The [metadata-mode crash window](commands.md) applies with special force to
containers: with `--log-capture metadata`, the unlink that removes agent
content from the volume happens at a **clean** stop. A SIGKILL'd container —
grace exhausted, OOM kill, node loss — is exactly that unclean death, and the
raw files stay on the volume. If agent output is sensitive, treat the volume as
sensitive, or run `--log-capture off` and accept the degraded sentinel
metering it announces.

## Provider keys in the host's memory

Under [key custody](run-keys.md#key-custody-preview--not-yet-supported) (a preview), `hekma serve` holds your provider keys in its own memory, never in the agents' environments. At start the host hardens that memory where the OS allows:

| OS | What the host does | What you should do |
|---|---|---|
| Linux | Marks itself non-dumpable and sets its core-dump limit to 0, so other processes running as the same user cannot read its memory or environment, and a crash writes no core file. If that fails, the host says so and keeps serving. | Nothing more. |
| macOS | Nothing. | Leave full crash reports off for `hekma`. |
| Windows | Nothing. | Leave full crash dumps off for `hekma`. |

The secrets file lives in the state directory, so backups, snapshots and sync tools copy it with the volume. Exclude it, or give `hekma serve` its keys through its own environment instead, which the resolver reads first.

## Held calls across a stop

A hold is stored with its Run, so it outlives the Service Host. A graceful stop answers each call the metering proxy is holding with `503 hekma_hold_interrupted` (`x-should-retry: true`); the SDK retries, and the next host holds the call again. A hard kill breaks the held connections; their calls are closed as `crash` at the next start, and the hold is back in place before the new proxy accepts a call.

## Inbound telemetry

The Control API listener also serves [`POST /v1/metrics`](telemetry.md#inbound-telemetry-v1metrics), OTLP/HTTP+JSON under a binding's announce credential. It is always on and does nothing until you issue an announce credential. It binds nothing new and follows the listener's TLS and loopback rules.

## Leases and the sweeper

The Service Host that owns the metering proxy also runs its **sweeper**: every 5 seconds it marks `silent` each [announced](announce.md) Run whose Lease lapsed and each Run without a Lease that had no traffic for 15 minutes, and ends each Run that stayed `silent` for 60 minutes (cause `silent-window`, its keys revoked). Silence never stops, pauses or fences a Run, and a supervised agent's Run is never swept: its process is its heartbeat. Only the engine that owns the proxy sweeps: a read-only open, such as `hekma ps`, never does. Statuses are stored, so a host restarted after a crash resumes from them: a Run that went silent before the crash is still silent, with no second event.

A Host embedding the engine sets the windows in `MeteringProxyOptions.leases` (`LeaseOptions { lease_secs, wire_inactivity_secs, silent_window_secs, sweep_interval_secs }`, defaults 60, 900, 3600 and 5). They are host settings, not Policy limits.

## Agents in their own Shim (a preview)

Set `HEKMA_SHIM=1` in the environment of `hekma serve` (or of the `hekma`
command that starts the host) and the host launches each agent through its
own small supervisor, a Shim, which this binary runs as the hidden internal
command `hekma shim`. The Shim owns the agent's process, so the agent keeps
running when the host exits or crashes, and the next host reconnects to it.
Unix only, and ACP agents stay held directly. It is off by default: without
the variable, nothing changes.

**Handoff Mode (a preview).** By default a stopping host stops every agent,
Shim-held or not. Start the host with `--handoff` (or `HEKMA_HANDOFF=on`) and
a stopping host — on SIGTERM, SIGINT or an auto-started host's idle exit —
instead releases each agent held through a Shim: it closes the connection
without sending a stop, so the agent keeps running in its Shim, and notes
`<name> released to its Shim`. The next host, started by you or by the next
`hekma` write, reconnects to each one. Agents held directly (ACP agents, or
any agent while `HEKMA_SHIM` is off) are still stopped, and the started
notice says so when Handoff Mode is on without Shims. After a plain stop,
nothing serves the metering proxy or the Control API until the next host
starts, and calls in that window are refused.

**Your service manager must leave the Shims alone.** Handoff Mode and
in-place upgrades keep agents running only if whatever stops the host stops
the host's own process and nothing else. When agents run in their own Shim,
the started notice says what Hekma could check, and `GET /v0/host` carries
it as `handoff_check`:

| Manager | Keeps the Shims | Stops them with the host | What Hekma reports |
|---|---|---|---|
| systemd | `KillMode=process` or `none` | `KillMode=control-group` (the default) or `mixed` | `verified` or `unsafe`, naming the unit and its setting |
| launchd | `AbandonProcessGroup=true` | the default | `unverified`, with how to check |
| A container | — | stopping the container ends every process in it | `unverified` |
| Windows | — | — | `unverified` (not checked yet) |

Hekma never refuses to start on `unsafe`: it warns, and stopping every agent
with the host remains a working configuration.

**After a crash.** A host that is killed (`kill -9`, out of memory, a power
cut) cannot stop anything. Each host records in the state directory, before
it adopts anything, which Handoff Mode it runs, and the next host acts on
what the dead one recorded:

| The crashed host ran | Its agents in their Shims (or detached) |
|---|---|
| `--handoff` (or `HEKMA_HANDOFF=on`) | Kept: the next host reconnects to each and says how many. |
| `--no-handoff` (or `HEKMA_HANDOFF=off`) | Cleaned up: the next host stops each one the ordinary way (the graceful signal, then the forced kill after the stop window), its Run ends with cause `engine-crash`, and the started notice names them. A detached start is never cleaned up: it was asked to outlive any host. |
| No mode chosen (the default), or a state directory from before this record | Kept: an unknown wish never stops anything, and the notice says so. |

Agents held directly die with the host only if the operating system takes
their process group with it; those that survive are treated the same way.

**In-place upgrades (a preview, Unix).** After installing a new `hekma`, run
`hekma serve --upgrade` (or `POST /v0/host/upgrade`) to move the running host
onto it without stopping agents or refusing a connection:

1. The host launches the new binary with its own arguments, and the two
   connect over a socket only your user can open, inside the state
   directory. A one-time token, passed in the environment and never written
   anywhere, proves the new process is the one this host launched.
2. Until the new host answers, nothing has stopped: if it fails to start or
   connect within 30 seconds, the old host notes why and keeps serving.
3. The old host then stops its Control API, releases the agents held through
   their Shims (whatever `--handoff` says), stops the agents held directly,
   and closes its store. It hands over the metering proxy's listening
   socket, which never closes, so a connection that arrives meanwhile waits
   rather than being refused, and its host lock, so no third host can take
   the state directory in between.
4. The new host opens the state directory, reconnects to every Shim, serves
   on the same socket, writes `control.json`, and tells the old host, which
   exits.

`hekma serve --upgrade` waits until `control.json` names the new host and
prints both pids. With no host serving the state directory it does nothing:
the next `hekma` command starts a host on the new binary anyway. If the new
host fails after the hand-over, the old one exits with an error saying so;
the agents keep running in their Shims and the next `hekma` command starts a
host that reconnects to them. Calls that were in flight or held at the
proxy when the old host stopped get the same retryable answer as at any
shutdown.

Each Shim also watches its agent's process tree for direct connections to
known provider hosts, every 5 seconds, and the host records what it finds on
the agent's Run: [bypass detection](run-keys.md#bypass-detection-preview).
The host resolves the provider-host list and hands it to each Shim; the Shim
itself does no DNS. A Shim keeps its findings while no host is connected and
hands them to the next host.

## Retention

The same host keeps the state directory from growing without bound. An ended Run keeps its full detail for seven days, then is reduced to its totals, labels and end cause; per-call records are kept for seven days, then their daily file is deleted whole. The host does this when it starts and then once a day. Change either window with `hekma run retention --runs <days>` or `--calls <days>`, or `PUT /v0/retention`, from 1 to 3650 days; usage records, events and the export outbox are not touched.

## The meter is cooperative — the container is not a fence

Engine-observed metering works inside the container as it does on a machine.
The host's [metering proxy](metering-proxy.md) is loopback-only, and the agent
and the host share the container's network namespace, so the injected
`base_url` (`<proxy url>/_hekma/runs/<run id>`) resolves. The proxy's port is
recorded in the state directory, so it stays the same across host restarts
(`--metering-listen <port>` pins it), and an adopted agent's injected address
stays live. Clients Hekma did not start reach the same proxy through an
[endpoint binding](metering-proxy.md#endpoint-bindings).

That loopback assumption has a sharp edge for **sidecars**: an agent in a
second container or pod cannot reach the injected `127.0.0.1` — it points
inside the agent's own network namespace. Such an agent either fails every
model call (connection refused — loud, at the agent) or ignores the injected
address and walks around the meter silently. Run the agent in the SAME
container as the host (the recipe below), or use self-reported metering for
that agent — observed metering is not a cross-container surface.

Now the caveat in plain words (NFR-6): **the observed meter is cooperative**.
It meters model traffic because the agent dials the `base_url` it was handed;
an agent that ignores the injected address walks around it. The container adds
**no fence** — it is not a sandbox, and neither is the loopback bind. A Host
that needs enforcement the agent cannot bypass keeps enforcement of record at
its own external choke point (a proxy or gateway it controls) and treats
Hekma's ledger as the cooperative record to reconcile against. The reasoning
is in [the metering design essay](design/metering-agents-you-dont-control.md).

## The wire fence: `block` refuses further model calls

`budget.breach_action = block` makes the [metering proxy](metering-proxy.md)
refuse a Run's model calls once a ceiling is reached: an engine-observed agent's,
and those of every Run whose calls go through the proxy, at any depth. Unlike `pause` and
`stop`, it does not change the agent's lifecycle state. The process keeps running,
and every further model call through the proxy gets a typed refusal that
common SDKs do not retry. The breach is recorded like any other (the breach event
carries `"action": "block"`), and the engine log notes that the fence is up.

```bash
hekma agent config set my-agent budget.tokens.cumulative 2000000
hekma agent config set my-agent budget.breach_action block
```

The rules:

- **It is never a default.** `block` applies only where you set it.
- **It needs the proxy.** `block` is refused at `config set` on an instance
  whose calls do not go through the metering proxy (a `self-reported`
  instance), and the error names the instance's metering source. If `block`
  reaches such an instance some other way, for example through a hand-edited
  config file, a breach is recorded, acts as `warn`, and the engine log says
  the fence could not be raised.
- **It lifts with the Run, or with a raised ceiling.** The fence lives as long
  as the Run. It lifts when the Run ends (`stop` then `start`), or as soon as
  the matching ceiling is raised above the totals on the live Run — through
  `hekma budget set`, an agent `budget.*` configuration write or a Policy
  change. It lifts reason by reason: a fence still holding another latched
  `block` reason stays up until that ceiling is raised too. A Run is checked
  before its process starts: if it already sits at or over a `block` ceiling
  (usually the cumulative one), the fence is up before its first call.
- **A relaunch keeps the fence.** A Restart Policy relaunch continues the same
  Run, so its per-run totals keep counting and a per-run `block` fence is up
  again before the relaunched process's first call.
- **Restarts and adoption keep it.** A host that restarts rebuilds every
  latched `block` from the store before the proxy serves a call, and the fence
  holds until a raise or reset re-arms it, even if the configured action
  changed meanwhile.
- **It covers the Runs beneath.** A Run's call is refused while the Run or any
  Run above it holds a latched `block`, so a breach on a parent fences the
  child Runs its agent opens.
- **Safe methods pass.** `GET`, `HEAD` and `OPTIONS` requests (model listings,
  probes) are forwarded. They carry no billable completion.
- **It is cooperative.** The fence refuses calls that go through the
  proxy. An agent that calls the provider directly is not fenced. Your own
  choke point stays the enforcement of record.

### The refusal (`proxy-wire v1`)

A fenced call is answered without being forwarded or metered:

- **Status `402 Payment Required`.** The OpenAI, Anthropic and Vercel AI SDKs
  retry 408, 409, 429 and 5xx, but not 402. Hekma avoids 403 because agents
  read it as a credential failure.
- **Headers:** `content-type: application/json` and `x-should-retry: false`
  (the OpenAI and Anthropic SDKs check this header before the status). There is
  no `retry-after`.
- **Body:** compact JSON that parses as both an OpenAI and an Anthropic error:

```json
{"type":"error","error":{"type":"hekma_budget_block","code":"tokens_cumulative","message":"Hekma budget fence: this agent's cumulative token ceiling is reached, so further model calls are refused. This is not retryable; the operator raises the ceiling or starts a new Run.","param":null,"dimension":"tokens","scope":"cumulative","proxy_wire":"v1"}}
```

`error.type` is always `hekma_budget_block`. `error.code` names the ceiling:
`tokens_per_run`, `tokens_cumulative`, `dollars_per_run` or
`dollars_cumulative`. When more than one ceiling is fenced, the first in that
order is reported. `dimension` and `scope` repeat the code in Hekma's own
spelling. The body never carries a limit, a total, an instance name, or any
request or response content. Key on `error.type` and `error.code`; the message
text may change.

This response is the first part of `proxy-wire v1`, the versioned wire contract
of Hekma's metering proxy. Later releases keep it unchanged. The governance
vectors pin the same refusal (the `fence` cases, from vectors 1.1.0), so a Host
guard can prove it answers the same way. See
[Governance conformance vectors](governance.md).

## A worked recipe: FROM scratch

The image below is deliberately minimal: a fetch stage downloads the release
archive, verifies it against the release's `.sha256`, and unpacks it; the final
stage starts from `scratch` and copies the three binaries out. Pin
`HEKMA_VERSION` to the release you are deploying at copy time — the asset list
on that release's page is the source of truth for which archives exist. The
fetch base is digest-pinned for the same reason; the digest below is a
point-in-time record, so re-verify it against the `alpine:3.22` tag when you
copy the recipe.

```dockerfile
# syntax=docker/dockerfile:1
ARG HEKMA_VERSION=vX.Y.Z

# Stage 1 — fetch and verify the release archive (needs a shell and curl).
# The digest is a point-in-time pin of the alpine:3.22 multi-arch index;
# re-pin at copy time when you bump the version.
FROM alpine:3.22@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8 AS fetch
ARG HEKMA_VERSION
RUN apk add --no-cache curl
WORKDIR /fetch
RUN set -eux; \
    archive="hekma-${HEKMA_VERSION}-x86_64-unknown-linux-musl.tar.gz"; \
    base="https://github.com/Ktesio/hekma/releases/download/${HEKMA_VERSION}"; \
    curl -fsSLO "${base}/${archive}"; \
    curl -fsSLO "${base}/${archive}.sha256"; \
    sha256sum -c "${archive}.sha256"; \
    mkdir /out; \
    tar -xzf "${archive}" -C /out

# Stage 2 — the supervisor image: three statically linked binaries, nothing else.
FROM scratch
COPY --from=fetch /out/hekma /hekma
COPY --from=fetch /out/hkm /hkm
COPY --from=fetch /out/hekma-host /hekma-host

# The state directory lives on a volume (see above).
ENV HEKMA_STATE_DIR=/data
VOLUME /data

# TERM is the container stop contract.
STOPSIGNAL SIGTERM

ENTRYPOINT ["/hekma-host"]
# Turn the Control API on: BOTH flags or neither. --bearer-secret names a
# secret, not a value — it resolves from the environment at startup, which is
# the natural container wiring (pass it with -e or a secret; never bake it
# into the image). Binding beyond loopback REQUIRES TLS: the PEM certificate
# and key come from a read-only mount (never baked into the image). --stop-window
# 45 pairs with the 90s grace below.
CMD ["--listen", "0.0.0.0:8477", "--bearer-secret", "CONTROL_BEARER", \
     "--tls-cert", "/certs/cert.pem", "--tls-key", "/certs/key.pem", \
     "--stop-window", "45"]
```

Build and run it — note the stop timeout is sized **past** the stop window
(45 s window, 90 s grace), and the API port is published on the operator's
loopback only:

```bash
docker build -t hekma-supervisor .
docker run -d --name hekma \
  -v hekma-data:/data \
  -e CONTROL_BEARER="use-a-long-random-value" \
  -v "$PWD/certs:/certs:ro" \
  -p 127.0.0.1:8477:8477 \
  --stop-timeout 90 \
  hekma-supervisor
```

On the Control API in a container: container recipes legitimately bind
`0.0.0.0` because the agent, the runtime's port publishing, or an in-network
proxy needs to reach the host — and **a non-loopback bind requires TLS**. The
host refuses `--listen 0.0.0.0:…` without `--tls-cert` and `--tls-key` (exit
code `2`, before anything binds or opens), and with them the listener answers
HTTPS only. Use a certificate your clients trust (`curl --cacert
certs/cert.pem https://127.0.0.1:8477/v0/version -H "Authorization: Bearer
…"`); the TLS stack is the vendored rustls + ring pair, so the static image
needs no system TLS library. Publishing the port on the operator machine's
loopback (`-p 127.0.0.1:8477:8477`) keeps it local as well. Every serve also
binds its own loopback local endpoint inside the container (recorded in
`/data/control.json`) — that is how a `hekma agent` command run inside the
container (`docker exec hekma /hekma agent list`) reaches the host. There
is no built-in health-probe surface yet — and the Control API is not one:
every route, `/v0/version` included, sits behind the bearer layer (an
unauthenticated probe gets `401`, not a health answer). The honest liveness
signal is the process being up and the `[hekma] ` notice stream on stderr.

For feeding the fleet's governance metrics to an observability stack, the
host has an opt-in OTLP export (`--otlp-endpoint`, an `http://` or `https://`
collector on loopback or in the network; the exporter does not exist without
the flag). The
versioned metric exposition and its container-relevant honesty rules
(bounded POSTs, the drop counter, the bounded shutdown flush) live in
[Telemetry export](telemetry.md).

To stop it, let the runtime do its part:

```bash
docker stop hekma   # SIGTERM -> full drain -> exit 0 (well inside the 90s grace)
docker logs hekma   # the per-instance drain outcomes, on stderr
```

`--metering-listen` is absent from the recipe on purpose: in the ENTRYPOINT
shape the agents are children of the container, so a host restart IS a
container restart, with no surviving observed agent to re-adopt. The proxy's
port is recorded in the state directory anyway. Pin it with
`--metering-listen` when a client in the same container that Hekma did not
start, reaching the proxy through an endpoint binding, needs a fixed address.
By default the proxy listens on `127.0.0.1`, so a sidecar or a published
container port cannot reach it. To serve clients outside the container, add
`--metering-listen 0.0.0.0:8400 --proxy-tls-cert /certs/proxy-cert.pem
--proxy-tls-key /certs/proxy-key.pem` (owner-only PEM files from a read-only
mount) and publish the port: every call from outside then needs a
proxy-access credential or a Run Key. See
[Beyond loopback](metering-proxy.md#beyond-loopback), and its port-squatting
note for shared machines.

The outbound identity headers (`x-hekma-run-id` and `x-hekma-labels`) are on
by default. `hekma proxy set identity-headers off` stores the setting in the
state directory, so it reaches the volume too: every container that mounts it
follows the setting without a flag in the ENTRYPOINT. `--no-identity-headers`
in the ENTRYPOINT overrides it for that container's run only. See
[Hekma in front of a gateway](gateways.md#turning-the-identity-headers-off).

If the grace is cut short instead, the metadata-mode crash window above is the
honest description of what remains on the volume — and the next container that
mounts it resumes from there.
