---
title: Telemetry export (OTLP)
description: The opt-in OTLP/HTTP+JSON metrics exporter — the versioned otlp-metrics v2 exposition (and the deprecated v1), its configuration flags, https collectors, the estimate honesty rule, and the drop/diagnostic behavior.
---

# Telemetry export (OTLP)

The Service Host can export its governance metrics — token usage, budget
breaches, derived cost, and lifecycle state — to the observability stack you
already run (Grafana, Datadog, or any OpenTelemetry-compatible collector)
over **OTLP/HTTP+JSON**. The exporter is a component of the host process: it
subscribes read-only to the engine's committed-event stream and POSTs
metric batches to a collector you name.

The privacy default is structural, not a toggle:

> **With no `--otlp-endpoint` configured, the exporter does not exist.** No
> task is spawned, no socket is dialed, nothing leaves the machine. This is
> asserted by test, not promised by documentation.

The export is **metrics-only**. No prompts, no responses, no agent content,
and no secrets ever ride a payload — the attribute vocabulary is closed
(instance names, Control Depth, Policy names, the label keys you allow-list,
budget words, state names, estimate labels; a Run id only inside exemplars).

## Inbound telemetry: `/v1/metrics`

> **On main, not yet released.** The receiver ships with the next release.

The export above sends Hekma's metrics out. The **receiver** takes a client's metrics in: a framework flow or plain client that only exports OpenTelemetry becomes an [`observed` Run](runs.md#observed), metered from its own token counts. It is the third way to [announce](announce.md) a Run, and the only one that never issues a Run Key.

`POST /v1/metrics` on the [Control API](control-api.md#inbound-telemetry) takes **OTLP/HTTP+JSON** under a binding's **announce credential** (`hekma binding credential issue <binding> --class announce`), sent as `authorization: Bearer hk_ann_…`. It rides the Control API listener and is always on. It does nothing until you issue an announce credential: the credential is the switch.

**What it reads.** Exactly three things, and nothing else is stored:

| Input | Where | Rule |
|---|---|---|
| The Run's identity | the resource attribute `hekma.run.external_id` | 1 to 128 visible ASCII characters. It is claimed beneath the credential's binding, the same identity an `x-hekma-run` split or an announce uses, so one identity is one Run on every channel. |
| Caller labels | resource attributes `hekma.label.<key>` | At most 16, each a valid label. A key the binding sets is skipped, never written. |
| Token usage | the metric `gen_ai.client.token_usage`, a **cumulative sum**, one point per `gen_ai.token.type` (`input`, `output` or `cached`) | The point's `gen_ai.request.model` attribute is read to price the usage, and never stored. |

**Cumulative counts, counted once.** OpenTelemetry SDKs export running totals. Hekma keeps the last total it saw per Run and token type, and records only the increase as usage, in the same transaction that updates the total. A batch sent twice therefore records nothing the second time, so a retry is always safe. A total that goes down means the client restarted: the new total is recorded as the increase, and the host notes it.

**What the usage is.** Each increase is one usage row with metering source `telemetry-reported`, priced like any Run the proxy found (the Run's Policy flat Rate, else the default Policy's entry for the model). Its dollars are always **estimated**. It counts toward the Run's budgets. An `observed` Run has no wire path and no process, so `warn` is the only Breach Action that executes there: a Policy's `block`, `pause` or `stop` acts as `warn` with the `depth-fallback` Anomaly. Telemetry never moves a Run's depth.

**One Run, one source.** A Run whose calls pass through the [metering proxy](metering-proxy.md) is metered per call. Its telemetry is skipped, so the two are never summed. The post still renews its Lease.

**Leases.** Every post is a sign of life: it renews the Run's Lease and brings a `silent` Run back to `active`. A Run whose telemetry stops goes `silent`, then ends after the silent window, as any announced Run does.

**Answers.**

| Answer | When |
|---|---|
| `200` | The batch was read. When points were skipped, the body is OTLP's `partialSuccess` with `rejectedDataPoints` and a fixed message naming why: no valid identity, an unread metric shape, a batch bound, or a Run metered by the proxy. |
| `400` | The body is not an OTLP JSON metrics request. |
| `401` | No credential, or an unknown or revoked one. |
| `403` | Any credential class other than an announce credential. |
| `409 too-many-children` | The binding already has 1,000 open announced Runs. |
| `413` | The body is over 4 MiB. Nothing is read. |
| `415` | The body is not JSON (`application/x-protobuf`, gRPC). |
| `503` | The state store did not take the batch within one second, with `x-should-retry: true`. Retrying is safe. |

A batch carries at most 8 resources and 64 token-usage points; the rest are skipped and counted. Batches without `hekma.run.external_id` create nothing.

**Protobuf and gRPC.** The receiver reads JSON only. To send from a client that exports protobuf or gRPC, run an OpenTelemetry Collector between them: its `otlp` receiver accepts any encoding, and its `otlphttp` exporter posts JSON to Hekma.

```yaml
receivers:
  otlp:
    protocols:
      grpc:
      http:
exporters:
  otlphttp/hekma:
    endpoint: http://127.0.0.1:8477
    encoding: json
    headers:
      authorization: "Bearer ${env:HEKMA_ANNOUNCE_CREDENTIAL}"
service:
  pipelines:
    metrics:
      receivers: [otlp]
      exporters: [otlphttp/hekma]
```

The client sets its resource attributes, for example `OTEL_RESOURCE_ATTRIBUTES=hekma.run.external_id=nightly-report,hekma.label.step=draft`.

## Configuration

```bash
hekma serve --otlp-endpoint http://127.0.0.1:4318
hekma serve --otlp-endpoint http://127.0.0.1:4318 \
  --otlp-headers authorization=secret:OTLP_AUTH \
  --otlp-flush-interval 30
hekma serve --otlp-endpoint https://otel.example.com \
  --otlp-label-keys team,env --otlp-ca-file internal-ca.pem
```

| Flag | Meaning |
|------|---------|
| `--otlp-endpoint <url>` | The collector's base URL, `http://` or `https://` (see [https collectors](#https-collectors)). Every flush POSTs to `<endpoint>/v1/metrics`. Refused shapes (a typed usage error before anything starts): a URL with a query string (`?`) or fragment (`#`), a non-numeric port, or a base that already ends in `/v1/metrics` — pass the base URL; the exporter appends the path. |
| `--otlp-headers <name=value,...>` | Headers attached to every POST (auth and friends). A value may be a `secret:NAME` reference — resolved once at startup from the environment, then the state directory's `secrets.toml`. Values are never logged; a reference that cannot be resolved refuses the startup, naming the secret. Control characters (CR/LF and friends) in a name or value, and duplicate names (case-insensitive), are refused at parse time. |
| `--otlp-flush-interval <secs>` | The flush cadence in whole seconds, `1..=3600`. Default `10`. Requires `--otlp-endpoint` — the exporter never half-exists. |
| `--otlp-exposition <v1\|v2>` | Which exposition to emit. Default `v2`; `v1` is [deprecated](#the-deprecated-exposition-otlp-metrics-v1). |
| `--otlp-label-keys <key,...>` | The label keys exported as `hekma.label.<key>` attributes in v2, at most 8. **None by default.** Refused with `v1`, which carries no label attributes. |
| `--otlp-label-cardinality <n>` | Each allow-listed key's distinct-value cap, `2..=10000`. Default `200`. Values past it fold into one series (see [the label cap](#the-label-cap)). |
| `--otlp-ca-file <pem>` | A PEM root certificate to trust for an `https://` collector, beside the built-in public roots. A file that cannot be read, or holds no certificate, refuses the start, naming it. |

The same flags exist on the `hekma-host` container binary. Every flag but
`--otlp-endpoint` requires `--otlp-endpoint` — the exporter never half-exists.

## The exposition: `otlp-metrics v2`

> **On main, not yet released.** v2 is the default from the next release; Hekma 0.11.0 emits v1.

The metric surface is **versioned**; the version rides the payload's
instrumentation scope (`otlp-metrics v2`). v2 groups series by what you
steer, never by Run, so the number of series stays flat however many Runs
there are:

- **No `hekma.run_id` attribute** on any data point. Per-Run detail rides
  **exemplars** instead: each token-usage and breach point carries up to 8
  exemplars, the 8 most recently observed distinct Runs, each with only
  `hekma.run_id` in `filteredAttributes`, a `timeUnixNano` and an `asInt`.
  There is no trace or span id (traces are not exported), no label value, no
  credential and no content.
- **`hekma.control_depth`**: `observed`, `wire`, `announced` or `supervised`,
  read from the event. A Run that runs supervised without a wire path is
  still `supervised`.
- **`hekma.policy`**, on families tied to a limit (`hekma.budget.breaches`):
  the name of the Policy that supplied the breached limit, `default` for the
  default Policy, or `explicit` for a limit set on the Run or its agent.
- **`hekma.label.<key>`** for the keys you allow-list (bound and Caller
  values alike), and none otherwise.
- **`hekma.instance`** when the Run has an Agent Instance. Runs Hekma did
  not start (wire and observed Runs) export too, without it. The **256
  most recently observed instances** keep their series; an older one leaves
  the payload, and a note names the count once per flush. That is a
  window, not a drop: `hekma.otlp.dropped` does not grow from it.

| Name | Type | Unit | Attributes | Notes |
|------|------|------|------------|-------|
| `gen_ai.client.token_usage` | histogram | `{token}` | `gen_ai.token.type`, `hekma.instance`?, `hekma.control_depth`, `hekma.label.*` | As in v1 (the same fixed bounds), with exemplars. |
| `hekma.budget.breaches` | sum | `{breach}` | `hekma.instance`?, `hekma.control_depth`, `hekma.policy`, `hekma.budget.dimension`, `hekma.budget.scope`, `hekma.budget.action`, `hekma.label.*` | +1 per recorded breach, with exemplars. |
| `hekma.usage.cost` | sum | `USD` | `hekma.instance`?, `hekma.control_depth`, **`hekma.estimate_state`**, `hekma.label.*` | As in v1. |
| `hekma.agent.state` | gauge | `{state}` | `hekma.instance`, `hekma.state` | As in v1. |
| `hekma.run.anomalies` | sum | `{anomaly}` | `hekma.anomaly`, `hekma.control_depth` | +1 each time a Run gains an Anomaly flag. |
| `hekma.calls.unscented` | sum | `{call}` | `outcome` (`served`/`refused`) | Calls no Hekma credential identified, under Scent-Check `shadow` (served) or `enforce` (refused). Always two series, whatever the calls sent. |
| `hekma.telemetry.received` | sum | `{point}` | `outcome` (`accepted`/`skipped`) | Inbound telemetry points on `/v1/metrics`. |
| `hekma.otlp.folded` | sum | `{observation}` | — | Label values folded past their key's cap. Always emitted. |
| `hekma.otlp.dropped` | sum | `{event}` | — | As in v1. Always emitted. |

`?` marks an attribute present only when the Run has an Agent Instance.
Bypass detection's counter joins when bypass detection ships.

### The label cap

Each allow-listed key keeps its first `--otlp-label-cardinality` distinct
values (200 by default). Every further value is exported as
`__hekma_overflow__`, one series for that key; each folded observation
counts on `hekma.otlp.folded`, and a note names the key once per flush. A
real value equal to `__hekma_overflow__` shares that series. The set of
values seen resets when the host restarts, as cumulative sums do.

### Moving from v1

- A dashboard that groups by `hekma.run_id` should group by
  `hekma.control_depth` and the label keys you allow-list
  (`--otlp-label-keys team,env`), and drill into a Run through the
  exemplars.
- The v1 series stop when the host restarts on v2, and v2's start fresh
  (cumulative sums reset at a restart in both).
- To keep v1 for now, start the host with `--otlp-exposition v1`.

## The deprecated exposition: `otlp-metrics v1`

> **Deprecated.** v1 stays selectable with `--otlp-exposition v1` for at least one minor release after v2 ships, and is removed only in a major release.

The sections below describe v1, which keys token usage by Run.

### Wire rules

The metric surface is **versioned**. The version marker is `otlp-metrics v1`
(it also rides the payload's instrumentation scope). Until the surface is
frozen, changes are additive only — a rename or a drop is an announced
release-surface change.

Wire rules (the standard opentelemetry-proto JSON mapping):

- lowerCamelCase keys (`resourceMetrics`, `timeUnixNano`, `asInt`);
- 64-bit integers as decimal strings (`timeUnixNano`, `asInt`, histogram
  `count` and `bucketCounts`);
- doubles as JSON numbers (`asDouble`, histogram `sum`/`min`/`max`/
  `explicitBounds`);
- `aggregationTemporality` numeric (`1` = CUMULATIVE) on sums; histograms
  omit the field (the proto's default temporality applies);
- the POST body is one `ExportMetricsServiceRequest` with
  `Content-Type: application/json`.

Resource attributes: `service.name=hekma-host`, `service.version`, and
`service.instance.id` — a stable identifier derived from the state directory
(the same root the single-writer host lock guards), never the raw path.

### Metric families

| Name | Type | Unit | Attributes | Notes |
|------|------|------|------------|-------|
| `gen_ai.client.token_usage` | histogram | `{token}` | `gen_ai.token.type` (`input`/`output`/`cached`), `hekma.instance`, `hekma.run_id` | One observation per committed usage event per known token type. `input` is inclusive of the cached subset. An UNKNOWN cached count yields no `cached` observation — never a fabricated zero. Fixed explicit bounds: **1000, 10 000, 100 000, 1 000 000, 10 000 000** tokens (six buckets). **Bounded run window:** the exporter keeps the **64 most-recently-observed runs per instance**; older run series leave the live payload (one `[hekma] ` diagnostic per flush cycle names the count when evictions happen). This is a window, not a drop — `hekma.otlp.dropped` never grows from it, and the durable ledger keeps every run. |
| `hekma.budget.breaches` | sum (cumulative, monotonic) | `{breach}` | `hekma.instance`, `hekma.budget.dimension` (`tokens`/`dollars`), `hekma.budget.scope` (`per-run`/`cumulative`), `hekma.budget.action` (`pause`/`stop`/`warn`/`block`) | +1 per recorded breach, regardless of the action taken. |
| `hekma.usage.cost` | sum (cumulative, monotonic) | `USD` | `hekma.instance`, **`hekma.estimate_state`** (mandatory) | The derived cost as `asDouble` (integer micro-dollars ÷ 1e6). See [the estimate honesty rule](#the-estimate-honesty-rule) and [the cost-series limitation](#the-cost-series-limitation). |
| `hekma.agent.state` | gauge | `{state}` | `hekma.instance`, `hekma.state` | `1` = the instance is in that lifecycle state, `0` = it left it. |
| `hekma.otlp.dropped` | sum (cumulative, monotonic) | `{event}` | — | The exporter's own drop counter — **always emitted, even at zero**. Counts engine events lost to bus lag or queue overflow, one per delivery batch a terminal failure dropped, and points a `200 partialSuccess` rejected. |

`gen_ai.client.operation_duration` is deliberately **absent**: no per-call
latency producer exists yet, and the exposition never fabricates a metric.
It joins additively when a producer lands.

### The cost-series limitation

`hekma.usage.cost` data points come **only from cumulative-scope dollar
breaches** — the moment the budget system records that an instance's lifetime
derived cost crossed its cap. A **per-run** dollar breach trips
`hekma.budget.breaches` but feeds no cost point (its observed total is a run
window, not the instance's lifetime cumulative), and a dollar breach without
an observed total yields no point either — never a fabricated `0.0` series.
The honest consequence: an instance that has **never breached its dollar cap
exports no cost series at all**, even while its token usage streams. Cost
series driven by ordinary usage events — not just breaches — needs an engine
payload extension (a per-usage-event cost field on the usage event); that is
a named deferral, reopening when a consumer needs cost series without dollar
breaches.

### The observation window

The exporter observes the engine's committed-event stream **from exporter
start to exporter stop**. Two windows are NOT observed, by design (not as
drops — they never entered the queue, and `hekma.otlp.dropped` never counts
them):

- **Adoption-time events before the exporter starts.** When the host opens a
  state directory that already has running agents, the engine's adoption
  events (and anything committed before serve started the exporter) are not
  part of the export — the cumulative series open at exporter start.
- **The shutdown-drain window.** After the exporter's bounded final flush,
  the drain's own stop-ladder events are not exported — the host is already
  exiting, and the export surface deliberately never holds it.

The durable ledger remains the lossless record for both windows.

### The estimate honesty rule

Every exported dollar figure carries its `hekma.estimate_state` attribute —
`estimated` (derived from self-reported tokens at a configured rate) or
`reconciled` (provider-confirmed actuals, once reconciliation ingestion
lands). The rule is structural: a reconciled figure is a **new time series**,
never a replacement of the estimated one — dashboards can watch both, and no
code path can export an unlabeled dollar.

### Restart-reset semantics

All sums and histograms are **cumulative from process start**. When the host
restarts, cumulative series restart from zero: counters undercount across a
crash window (delivery is at-most-once there — the event bus's documented
crash semantics), and the durable ledger remains the lossless record. The
export surface is a live dashboard feed, not a billing source; lossless
ledger-sourced re-derivation is a named future direction, not a promise this
surface makes quietly.

## Failure behavior: visible, never blocking

The exporter never delays supervision or enforcement:

- **Bounded POSTs.** Every delivery attempt carries an explicit timeout;
  the export path cannot hang the host.
- **Bounded retries.** `429`/`503`/`504` responses and attempt timeouts are
  retried within a small fixed budget (three attempts, short backoff).
  Everything else — connection refused, any other status — is terminal for
  that batch.
- **Every loss is visible.** A dropped batch, a bus lag, or a queue overflow
  surfaces a `[hekma] ` diagnostic on the host's stderr stream AND grows the
  `hekma.otlp.dropped` counter. A silent drop is a design bug, not a mode.
  Under a SUSTAINED outage the overflow diagnostic is rate-limited to once
  per flush cycle (the counter stays exact); a `200` whose
  `partialSuccess` reports rejected points is surfaced too — the rejection
  count and the collector's message are named, and the points join the
  dropped counter. Response bodies are drained (bounded) after every POST so
  pooled connections survive.
- **The queue is bounded.** Under a collector outage the exporter drops the
  OLDEST events (flat, known memory ceiling) and says so. The token-usage
  run series are bounded the same way — the 64 most-recent runs per
  instance (see the metric-families table).
- **Shutdown is bounded.** On the shutdown signal the exporter stops
  before/with the Control API, attempts one bounded final flush, and never
  holds the host's exit past that bound. In-flight points the final flush
  could not deliver are named on stderr (cumulative sums reset at restart —
  the documented restart-reset above).
- **No durable telemetry.** The exporter keeps no files and no cursors; the
  Usage Ledger's outbox remains the only durable export surface.

## https collectors

> **On main, not yet released.** Hekma 0.11.0 accepts `http://` collectors only.

`--otlp-endpoint` takes `https://` as well as `http://`. The exporter trusts
the built-in public root certificates (never the system store) and, with
`--otlp-ca-file`, a PEM root of your own, such as an internal CA or a
self-signed collector. TLS 1.2 is the minimum. A certificate the exporter
cannot verify fails the delivery like any terminal error: the batch is not
delivered, a note says so, `hekma.otlp.dropped` grows, and the next flush
tries again. A local collector accepting `http://` on loopback and exporting
TLS upstream remains a fine posture.

## Grafana and Datadog

Any OTLP/HTTP+JSON ingest works:

- **Grafana Alloy / the OTel Collector**: point Hekma's `--otlp-endpoint`
  at the collector's OTLP/HTTP receiver URL (Hekma is the CLIENT — it POSTs
  to the collector; nothing is received by the host); the metric families
  above then appear under their names verbatim.
- **Datadog**: the Datadog Agent's OTLP ingest (`/v1/metrics`) accepts the
  same payload.

The host's container posture (volume, shutdown, metering) lives in
[the hosting guide](hosting.md).
