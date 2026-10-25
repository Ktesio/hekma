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
| `hekma.budget.breaches` | sum (cumulative, monotonic) | `{breach}` | `hekma.instance`, `hekma.budget.dimension` (`tokens`/`dollars`), `hekma.budget.scope` (`per-run`/`cumulative`), `hekma.budget.action` (`pause`/`stop`/`warn`) | +1 per recorded breach, regardless of the action taken. |
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
