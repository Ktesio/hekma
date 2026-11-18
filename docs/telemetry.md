---
title: Telemetry export (OTLP)
description: The opt-in OTLP/HTTP+JSON metrics exporter — the versioned otlp-metrics v1 exposition, its configuration flags, the estimate honesty rule, and the drop/diagnostic behavior.
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
(instance and run identifiers, budget words, state names, estimate labels).

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
```

| Flag | Meaning |
|------|---------|
| `--otlp-endpoint <url>` | The collector's base URL. **http:// only** — see [the https deferral](#the-https-deferral). Every flush POSTs to `<endpoint>/v1/metrics`. Refused shapes (a typed usage error before anything starts): a URL with a query string (`?`) or fragment (`#`), a non-numeric port, or a base that already ends in `/v1/metrics` — pass the base URL; the exporter appends the path. |
| `--otlp-headers <name=value,...>` | Headers attached to every POST (auth and friends). A value may be a `secret:NAME` reference — resolved once at startup from the environment, then the state directory's `secrets.toml`. Values are never logged; a reference that cannot be resolved refuses the startup, naming the secret. Control characters (CR/LF and friends) in a name or value, and duplicate names (case-insensitive), are refused at parse time. |
| `--otlp-flush-interval <secs>` | The flush cadence in whole seconds, `1..=3600`. Default `10`. Requires `--otlp-endpoint` — the exporter never half-exists. |

The same three flags exist on the `hekma-host` container binary. `--otlp-headers`
and `--otlp-flush-interval` each require `--otlp-endpoint` — the exporter never
half-exists.

## The exposition: `otlp-metrics v1`

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

## The https deferral

v1 accepts **http:// endpoints only**. `https://` is a documented deferral,
not an oversight: the tested surface must equal the shipped surface, and TLS
handshakes are not provably testable network-free in this repository's
hermetic suite. The standard bridge — an OpenTelemetry Collector (or any
local forwarding collector) accepting http:// on loopback and exporting
TLS upstream — works today and is the recommended posture. The deferral
reopens when an operator names an https collector as a real need.

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
