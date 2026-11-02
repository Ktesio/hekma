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

## The meter is cooperative — the container is not a fence

Engine-observed metering works inside the container as it does on a machine:
the metering listener is loopback-only, the agent and the host share the
container's network namespace, so the injected `base_url` resolves — and
`--metering-listen <port>` pins the port so an adopted instance's injected
address survives a host restart.

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

`budget.breach_action = block` makes the metering listener refuse an
engine-observed agent's model calls once a ceiling is reached. Unlike `pause` and
`stop`, it does not change the agent's lifecycle state. The process keeps running,
and every further model call through the listener gets a typed refusal that
common SDKs do not retry. The breach is recorded like any other (the breach event
carries `"action": "block"`), and the engine log notes that the fence is up.

```bash
hekma agent config set my-agent budget.tokens.cumulative 2000000
hekma agent config set my-agent budget.breach_action block
```

The rules:

- **It is never a default.** `block` applies only where you set it.
- **It needs the listener.** `block` is refused at `config set` on an instance
  that does not meter through the engine-observed listener (a `self-reported`
  instance), and the error names the instance's metering source. If `block`
  reaches such an instance some other way, for example through a hand-edited
  config file, a breach is recorded, acts as `warn`, and the engine log says
  the fence could not be raised.
- **It lifts with the Run.** The fence lives as long as the Run's listener.
  It lifts when the Run ends (`stop` then `start`, or a Restart Policy relaunch,
  which starts a new Run). A new Run is checked before its process starts: if
  it already sits at or over a `block` ceiling (usually the cumulative one), the
  fence is up before its first call. To un-fence, raise the ceiling, then start
  a new Run. Un-fencing a live Run when its ceiling is raised is planned for a
  later release.
- **A relaunch does not carry a per-run fence.** A Restart Policy relaunch starts
  a fresh per-run budget, and the engine log says so when the replaced Run was
  fenced. Pair a per-run `block` ceiling with a cumulative one to bound an agent
  that crashes and is relaunched.
- **Restarts and adoption keep it.** A host that restarts and adopts the
  instance on the same `--metering-listen` port rebuilds the fence from the
  breach log before serving traffic.
- **Safe methods pass.** `GET`, `HEAD` and `OPTIONS` requests (model listings,
  probes) are forwarded. They carry no billable completion.
- **It is cooperative.** The fence refuses calls that go through the
  listener. An agent that calls the provider directly is not fenced. Your own
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
# into the image). --stop-window 45 pairs with the 90s grace below.
CMD ["--listen", "0.0.0.0:8477", "--bearer-secret", "CONTROL_BEARER", "--stop-window", "45"]
```

Build and run it — note the stop timeout is sized **past** the stop window
(45 s window, 90 s grace), and the API port is published on the operator's
loopback only:

```bash
docker build -t hekma-supervisor .
docker run -d --name hekma \
  -v hekma-data:/data \
  -e CONTROL_BEARER="use-a-long-random-value" \
  -p 127.0.0.1:8477:8477 \
  --stop-timeout 90 \
  hekma-supervisor
```

On the Control API in a container: a **non-loopback bind** works — container
recipes legitimately bind `0.0.0.0` because the agent, the runtime's port
publishing, or an in-network proxy needs to reach the host — but the surface
stays **bearer-only with no TLS**; do not expose it on an untrusted network
(the host warns loudly on any non-loopback bind). Publishing the port on the
operator machine's loopback (`-p 127.0.0.1:8477:8477`) keeps it local. There
is no built-in health-probe surface yet — and the Control API is not one:
every route, `/v0/version` included, sits behind the bearer layer (an
unauthenticated probe gets `401`, not a health answer). The honest liveness
signal is the process being up and the `[hekma] ` notice stream on stderr.

For feeding the fleet's governance metrics to an observability stack, the
host has an opt-in OTLP export (`--otlp-endpoint`, http:// on loopback or an
in-network collector; the exporter does not exist without the flag). The
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
container restart — there is no surviving observed agent to re-adopt, and the
fixed-port re-bind pin only earns its keep in shapes where agents outlive the
host process.

If the grace is cut short instead, the metadata-mode crash window above is the
honest description of what remains on the volume — and the next container that
mounts it resumes from there.
