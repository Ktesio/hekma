//! The OTLP/HTTP+JSON metrics payload — the `otlp-metrics v1` exposition
//! (AD-21): metric derivation from the engine's committed
//! [`EngineEvent`] stream, the serde wire structs mirroring the
//! opentelemetry-proto JSON mapping, and the unit tests that pin every
//! family without I/O.
//!
//! Wire rules (the frozen mapping, verified against opentelemetry-proto +
//! OTEP-0122):
//! * lowerCamelCase keys (`resourceMetrics`, `timeUnixNano`, `asInt`, ...);
//! * 64-bit ints as DECIMAL STRINGS (`timeUnixNano`, `startTimeUnixNano`,
//!   `asInt`, histogram `count`, `bucketCounts`);
//! * doubles as JSON numbers (`asDouble`, histogram `sum`/`min`/`max`/
//!   `explicitBounds`);
//! * `aggregationTemporality` numeric, `1` = CUMULATIVE — carried on SUMS,
//!   omitted on HISTOGRAMS (the proto's default temporality applies there);
//! * the POST body is one `ExportMetricsServiceRequest`.
//!
//! Derivation rules (the versioned exposition — additive-only-until-frozen):
//! * [`EngineEvent::UsageUpdate`] → `gen_ai.client.token_usage` histogram
//!   observations, one per token type present (input / output / cached);
//!   `gen_ai.client.operation_duration` is NEVER fabricated (no producer).
//! * [`EngineEvent::BudgetBreach`] → `hekma.budget.breaches` sum (+1 per
//!   recorded breach, dimension/scope/action attributes); a CUMULATIVE-
//!   scope DOLLAR breach with an observed total also feeds `hekma.usage.cost`
//!   (cumulative sum, unit USD, asDouble = micros/1e6, MANDATORY
//!   `hekma.estimate_state` attribute — a reconciled figure is a new series,
//!   never a replacement of the estimated one). A PER-RUN dollar breach trips
//!   the breach counter only (its observed total is a run window, not the
//!   instance's lifetime cumulative — feeding the cumulative cost series from
//!   it would understate and then "max" against the real total); a dollar
//!   breach whose `dollar_observed` is absent yields NO cost point — the
//!   honest absence, never a fabricated `0.0` series.
//! * [`EngineEvent::Transition`] → `hekma.agent.state` gauge by state
//!   (1 = the instance is in that state, 0 = it left it).
//! * exporter-side losses (bus lag, queue overflow, a terminally-dropped
//!   batch, collector-rejected points) → `hekma.otlp.dropped` — the
//!   self-metric that keeps a silent drop impossible.
//!
//! NO content and NO secrets ride any payload: attributes carry only
//! instance/run identifiers, budget vocabulary, and estimate labels —
//! never prompts, responses, or resolved secret values (the no-leak matrix
//! extends to the export path).

use std::collections::BTreeMap;

use serde::Serialize;

use hekma_engine::{BreachAction, BreachDimension, BreachScope, EngineEvent, EstimateLabel};

/// The exposition's name — the versioned marker `docs/telemetry.md` pins.
pub const EXPOSITION_NAME: &str = "otlp-metrics";

/// The exposition version this module emits. ADDITIVE-ONLY until frozen: a
/// rename or drop is a release-surface change (AI-55 review discipline).
pub const EXPOSITION_VERSION: &str = "v1";

/// The instrumentation scope the payload stamps (the exporting component).
pub const SCOPE_NAME: &str = "hekma-host.otlp-metrics";

/// Numeric CUMULATIVE aggregation temporality (the proto's enum value) —
/// carried on every SUM this module emits.
pub const AGGREGATION_TEMPORALITY_CUMULATIVE: i32 = 1;

/// The token histogram's FIXED explicit bounds (tokens per observation) —
/// coarse, stable, documented in `docs/telemetry.md`; enough to see runaway
/// batches. Six buckets (the bounds plus the implicit +Inf bucket).
pub const TOKEN_HISTOGRAM_BOUNDS: [f64; 5] =
    [1_000.0, 10_000.0, 100_000.0, 1_000_000.0, 10_000_000.0];

/// Micro-dollars per dollar — the scale factor turning the engine's integer
/// [`hekma_engine::Micros`] into the USD `asDouble` the cost sum carries.
const MICROS_PER_DOLLAR_F64: f64 = 1_000_000.0;

/// The per-instance RUN-SERIES bound (P5): the token histogram keeps at most
/// this many runs per instance, the most-recently-observed ones — a
/// long-lived host running thousands of runs must keep memory and payload
/// flat. Evicted runs are a bounded WINDOW (documented in
/// `docs/telemetry.md`), not drops: they never touch `hekma.otlp.dropped`.
pub const MAX_RUNS_PER_INSTANCE: usize = 64;

/// Which token count an observation carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TokenType {
    /// Prompt tokens (INCLUSIVE of the cached subset — the ledger invariant).
    Input,
    /// Completion tokens.
    Output,
    /// The cached subset of the input (known counts only — an UNKNOWN cached
    /// count yields NO cached observation, never a fabricated zero).
    Cached,
}

impl TokenType {
    /// The `gen_ai.token.type` attribute value (the semconv vocabulary).
    pub fn as_str(self) -> &'static str {
        match self {
            TokenType::Input => "input",
            TokenType::Output => "output",
            TokenType::Cached => "cached",
        }
    }
}

/// One histogram accumulator: the cumulative count/sum/min/max and the
/// bucket counts for one (instance, run, token type) series.
#[derive(Clone, Debug, Default)]
struct HistogramAccumulator {
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
    buckets: [u64; TOKEN_HISTOGRAM_BOUNDS.len() + 1],
}

impl HistogramAccumulator {
    /// Record one observation, bucketing it against [`TOKEN_HISTOGRAM_BOUNDS`].
    fn observe(&mut self, value: f64) {
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.count = self.count.saturating_add(1);
        self.sum += value;
        let mut bucket = self.buckets.len() - 1; // the implicit +Inf bucket
        for (index, bound) in TOKEN_HISTOGRAM_BOUNDS.iter().enumerate() {
            if value <= *bound {
                bucket = index;
                break;
            }
        }
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
    }
}

/// The budget-breach counter's series key: which dimension tripped, in which
/// scope, with which action, for which instance.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BreachKey {
    instance: String,
    dimension: &'static str,
    scope: &'static str,
    action: &'static str,
}

/// The `hekma.usage.cost` series key: the instance plus the MANDATORY
/// estimate label — a reconciled figure is its own series, never a
/// replacement of the estimated one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CostKey {
    instance: String,
    estimate_state: &'static str,
}

/// The exporter's cumulative metric state: everything the committed event
/// stream has fed it since process start (the honest restart-reset —
/// cumulative sums reset at restart, documented in `docs/telemetry.md`).
#[derive(Clone, Debug)]
pub struct MetricState {
    start_time_unix_nano: u64,
    service_instance_id: String,
    token_usage: BTreeMap<(String, String, TokenType), HistogramAccumulator>,
    /// Per (instance, run): the monotonic sequence of the LAST observation —
    /// the recency order the [`MAX_RUNS_PER_INSTANCE`] window evicts by (the
    /// BTreeMap's lexical run order is not recency).
    run_last_seen: BTreeMap<(String, String), u64>,
    /// The next recency sequence stamp.
    next_run_seq: u64,
    /// Run series evicted by the window SINCE the task last drained the
    /// per-cycle count (the once-per-flush-cycle note's source, P5).
    evicted_runs_this_cycle: BTreeMap<String, u64>,
    breaches: BTreeMap<BreachKey, u64>,
    agent_state: BTreeMap<(String, String), i64>,
    cost: BTreeMap<CostKey, f64>,
    dropped: u64,
    in_flight: u64,
}

impl MetricState {
    /// Fresh state: `start_time_unix_nano` stamps every data point's
    /// `startTimeUnixNano` (process start — the cumulative window's open),
    /// `service_instance_id` rides the resource (the state-dir lock identity).
    pub fn new(start_time_unix_nano: u64, service_instance_id: String) -> Self {
        MetricState {
            start_time_unix_nano,
            service_instance_id,
            token_usage: BTreeMap::new(),
            run_last_seen: BTreeMap::new(),
            next_run_seq: 0,
            evicted_runs_this_cycle: BTreeMap::new(),
            breaches: BTreeMap::new(),
            agent_state: BTreeMap::new(),
            cost: BTreeMap::new(),
            dropped: 0,
            in_flight: 0,
        }
    }

    /// Fold one committed engine event into the state (pure — no I/O).
    pub fn observe(&mut self, event: &EngineEvent) {
        self.in_flight = self.in_flight.saturating_add(1);
        match event {
            EngineEvent::Transition(transition) => {
                // Gauge by state: 1 for the state the instance ENTERED,
                // 0 for the one it LEFT.
                let instance = transition.instance.clone();
                self.agent_state.insert(
                    (
                        instance.clone(),
                        transition.prior_state.as_str().to_string(),
                    ),
                    0,
                );
                self.agent_state
                    .insert((instance, transition.new_state.as_str().to_string()), 1);
            }
            EngineEvent::BudgetBreach(breach) => {
                let key = BreachKey {
                    instance: breach.instance.clone(),
                    dimension: dimension_str(breach.dimension),
                    scope: scope_str(breach.scope),
                    action: action_str(breach.action),
                };
                let count = self.breaches.entry(key).or_insert(0);
                *count = count.saturating_add(1);
                if breach.dimension == BreachDimension::Dollars
                    // CUMULATIVE scope only (P1a): a per-run dollar breach's
                    // observed total is a RUN window, not the instance's
                    // lifetime cumulative — the breach counter above still
                    // records it with its scope attribute.
                    && breach.scope == BreachScope::Cumulative
                {
                    // An absent observed total yields NO cost point (P1b) —
                    // the honest absence, never a fabricated 0.0 series.
                    if let Some(micros) = breach.dollar_observed {
                        // The MANDATORY label — an unlabeled dollar is a bug
                        // this module refuses to write (the event's label is
                        // the only source; a missing label on a dollars
                        // breach cannot be expressed by the type, so the arm
                        // is total).
                        let label = breach
                            .estimate_label
                            .map(estimate_str)
                            .unwrap_or(EstimateLabel::Estimated.as_str());
                        let observed = micros.get().max(0) as f64 / MICROS_PER_DOLLAR_F64;
                        let key = CostKey {
                            instance: breach.instance.clone(),
                            estimate_state: label,
                        };
                        let entry = self.cost.entry(key).or_insert(0.0);
                        // Cumulative-sum semantics: the breach event carries
                        // the instance's committed total at cap time, so the
                        // series takes the MAX (monotonic; never regresses).
                        *entry = entry.max(observed);
                    }
                }
            }
            EngineEvent::UsageUpdate(update) => {
                let event = &update.event;
                let run = event.run_id.as_str().to_string();
                // Stamp the run's recency FIRST (P5): the window keeps the
                // most-recently-OBSERVED runs, so every usage event refreshes
                // its run regardless of which token types it carries.
                let seq = self.next_run_seq;
                self.next_run_seq = self.next_run_seq.saturating_add(1);
                self.run_last_seen
                    .insert((event.instance.clone(), run.clone()), seq);
                for (token_type, value) in [
                    (TokenType::Input, Some(event.input_tokens)),
                    (TokenType::Output, Some(event.output_tokens)),
                    (
                        TokenType::Cached,
                        // UNKNOWN (None) yields no observation — the honest
                        // absence; Some(n) is a real observation even at 0.
                        event.cached_tokens,
                    ),
                ] {
                    if let Some(tokens) = value {
                        self.token_usage
                            .entry((event.instance.clone(), run.clone(), token_type))
                            .or_default()
                            .observe(tokens as f64);
                    }
                }
                self.enforce_run_window(&event.instance);
            }
        }
    }

    /// Enforce the [`MAX_RUNS_PER_INSTANCE`] window for `instance` (P5):
    /// evict the least-recently-observed runs beyond the bound — every token
    /// series of the evicted run leaves the map, so memory and payload stay
    /// flat on a long-lived host. Evictions are a bounded window, NOT drops:
    /// `hekma.otlp.dropped` never grows here; the per-cycle count the task's
    /// once-per-flush note names is accumulated in `evicted_runs_this_cycle`.
    fn enforce_run_window(&mut self, instance: &str) {
        let mut runs: Vec<(String, u64)> = self
            .run_last_seen
            .range((instance.to_string(), String::new())..)
            .take_while(|((inst, _), _)| inst == instance)
            .map(|((_, run), seq)| (run.clone(), *seq))
            .collect();
        if runs.len() <= MAX_RUNS_PER_INSTANCE {
            return;
        }
        // Evict the least-recently-observed runs beyond the bound.
        runs.sort_by_key(|(_, seq)| *seq);
        let excess = runs.len() - MAX_RUNS_PER_INSTANCE;
        for (run, _) in runs.into_iter().take(excess) {
            self.run_last_seen
                .remove(&(instance.to_string(), run.clone()));
            for token_type in [TokenType::Input, TokenType::Output, TokenType::Cached] {
                self.token_usage
                    .remove(&(instance.to_string(), run.clone(), token_type));
            }
            *self
                .evicted_runs_this_cycle
                .entry(instance.to_string())
                .or_insert(0) += 1;
        }
    }

    /// Drain the per-cycle evicted-run counts for the task's once-per-flush
    /// note (P5): returns (instance, evicted-since-last-drain) pairs, then
    /// resets the accumulator — the NEXT flush cycle's note is its own.
    pub fn take_evicted_run_series(&mut self) -> Vec<(String, u64)> {
        let drained = self
            .evicted_runs_this_cycle
            .iter()
            .map(|(instance, count)| (instance.clone(), *count))
            .collect();
        self.evicted_runs_this_cycle.clear();
        drained
    }

    /// Record `n` engine events lost to bus lag — surfaced (AI-18) through
    /// both the note the task emits and this self-metric.
    pub fn note_dropped_events(&mut self, n: u64) {
        self.dropped = self.dropped.saturating_add(n);
    }

    /// Record ONE terminally-dropped delivery batch (the payload never
    /// reached the collector; the next flush re-exports the cumulative state,
    /// so only the batch — not the observations — is counted).
    pub fn note_dropped_batch(&mut self) {
        self.dropped = self.dropped.saturating_add(1);
    }

    /// Count `n` metric points the collector REJECTED in a 200's
    /// `partialSuccess` (P3b) — delivered-but-rejected is surfaced through
    /// the note the task emits AND this self-metric, never silent.
    pub fn note_rejected_points(&mut self, n: u64) {
        self.dropped = self.dropped.saturating_add(n);
    }

    /// How many events were observed since the last successful delivery —
    /// the honest "in-flight" count the shutdown note names when the final
    /// flush does not land (lost points; cumulative sums reset at restart).
    pub fn in_flight(&self) -> u64 {
        self.in_flight
    }

    /// Mark every observation as delivered (a successful POST).
    pub fn mark_delivered(&mut self) {
        self.in_flight = 0;
    }

    /// The exporter's own drop counter's current value (the task asserts on
    /// growth in tests without decoding a full payload).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Build the OTLP/HTTP+JSON request body at `time_unix_nano` (the flush
    /// moment — every data point's `timeUnixNano`). Pure: borrows the state,
    /// builds the wire structs; `serde_json` renders them. The observation
    /// moment is GUARDED against a backward clock: `now = max(now, start)` —
    /// a `timeUnixNano` before `startTimeUnixNano` would export an inverted
    /// (start > end) cumulative window, so the clock can never regress the
    /// window past its own open.
    pub fn snapshot(&self, time_unix_nano: u64) -> ExportMetricsServiceRequest {
        let start = self.start_time_unix_nano.to_string();
        let now = time_unix_nano.max(self.start_time_unix_nano).to_string();
        let mut metrics = Vec::new();

        // gen_ai.client.token_usage — histogram, fixed documented bounds.
        if !self.token_usage.is_empty() {
            let data_points = self
                .token_usage
                .iter()
                .map(|((instance, run, token_type), accum)| HistogramDataPoint {
                    attributes: vec![
                        attr("gen_ai.token.type", token_type.as_str()),
                        attr("hekma.instance", instance),
                        attr("hekma.run_id", run),
                    ],
                    start_time_unix_nano: start.clone(),
                    time_unix_nano: now.clone(),
                    count: accum.count.to_string(),
                    sum: accum.sum,
                    bucket_counts: accum.buckets.iter().map(|c| c.to_string()).collect(),
                    explicit_bounds: TOKEN_HISTOGRAM_BOUNDS.to_vec(),
                    min: Some(accum.min),
                    max: Some(accum.max),
                })
                .collect();
            metrics.push(Metric {
                name: "gen_ai.client.token_usage".to_string(),
                description: Some(
                    "Tokens per committed usage observation, by token type (input is \
                     inclusive of the cached subset)"
                        .to_string(),
                ),
                unit: Some("{token}".to_string()),
                sum: None,
                gauge: None,
                histogram: Some(Histogram {
                    data_points,
                    // The proto's DEFAULT temporality applies to histograms —
                    // the field is omitted, never guessed at.
                    aggregation_temporality: None,
                }),
            });
        }

        // hekma.budget.breaches — cumulative sum, +1 per recorded breach.
        if !self.breaches.is_empty() {
            let data_points = self
                .breaches
                .iter()
                .map(|(key, count)| NumberDataPoint {
                    attributes: vec![
                        attr("hekma.instance", &key.instance),
                        attr("hekma.budget.dimension", key.dimension),
                        attr("hekma.budget.scope", key.scope),
                        attr("hekma.budget.action", key.action),
                    ],
                    start_time_unix_nano: start.clone(),
                    time_unix_nano: now.clone(),
                    as_int: Some(count.to_string()),
                    as_double: None,
                })
                .collect();
            metrics.push(Metric {
                name: "hekma.budget.breaches".to_string(),
                description: Some(
                    "Recorded budget breaches, regardless of the action taken".to_string(),
                ),
                unit: Some("{breach}".to_string()),
                sum: Some(Sum {
                    data_points,
                    aggregation_temporality: AGGREGATION_TEMPORALITY_CUMULATIVE,
                    is_monotonic: true,
                }),
                gauge: None,
                histogram: None,
            });
        }

        // hekma.usage.cost — cumulative sum, USD, asDouble = micros/1e6,
        // MANDATORY estimate_state attribute; reconciled is a new series.
        if !self.cost.is_empty() {
            let data_points = self
                .cost
                .iter()
                .map(|(key, usd)| NumberDataPoint {
                    attributes: vec![
                        attr("hekma.instance", &key.instance),
                        attr("hekma.estimate_state", key.estimate_state),
                    ],
                    start_time_unix_nano: start.clone(),
                    time_unix_nano: now.clone(),
                    as_int: None,
                    as_double: Some(*usd),
                })
                .collect();
            metrics.push(Metric {
                name: "hekma.usage.cost".to_string(),
                description: Some(
                    "Derived cumulative cost, labeled estimated or reconciled — never \
                     an unlabeled dollar"
                        .to_string(),
                ),
                unit: Some("USD".to_string()),
                sum: Some(Sum {
                    data_points,
                    aggregation_temporality: AGGREGATION_TEMPORALITY_CUMULATIVE,
                    is_monotonic: true,
                }),
                gauge: None,
                histogram: None,
            });
        }

        // hekma.agent.state — gauge by state (1 = in the state, 0 = left it).
        if !self.agent_state.is_empty() {
            let data_points = self
                .agent_state
                .iter()
                .map(|((instance, state), value)| NumberDataPoint {
                    attributes: vec![attr("hekma.instance", instance), attr("hekma.state", state)],
                    start_time_unix_nano: start.clone(),
                    time_unix_nano: now.clone(),
                    as_int: Some(value.to_string()),
                    as_double: None,
                })
                .collect();
            metrics.push(Metric {
                name: "hekma.agent.state".to_string(),
                description: Some(
                    "Lifecycle state per instance: 1 = in the state, 0 = left it".to_string(),
                ),
                unit: Some("{state}".to_string()),
                sum: None,
                gauge: Some(Gauge { data_points }),
                histogram: None,
            });
        }

        // hekma.otlp.dropped — ALWAYS emitted (even at zero): a silent drop
        // counter would defeat its own purpose.
        metrics.push(Metric {
            name: "hekma.otlp.dropped".to_string(),
            description: Some(
                "Exporter losses: bus lag, queue overflow, dropped batches, \
                 collector-rejected points"
                    .to_string(),
            ),
            unit: Some("{event}".to_string()),
            sum: Some(Sum {
                data_points: vec![NumberDataPoint {
                    attributes: Vec::new(),
                    start_time_unix_nano: start.clone(),
                    time_unix_nano: now.clone(),
                    as_int: Some(self.dropped.to_string()),
                    as_double: None,
                }],
                aggregation_temporality: AGGREGATION_TEMPORALITY_CUMULATIVE,
                is_monotonic: true,
            }),
            gauge: None,
            histogram: None,
        });

        ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: Resource {
                    attributes: vec![
                        attr("service.name", "hekma-host"),
                        attr("service.version", env!("CARGO_PKG_VERSION")),
                        attr("service.instance.id", &self.service_instance_id),
                    ],
                },
                scope_metrics: vec![ScopeMetrics {
                    scope: InstrumentationScope {
                        name: SCOPE_NAME.to_string(),
                        version: EXPOSITION_VERSION.to_string(),
                    },
                    metrics,
                }],
            }],
        }
    }
}

fn dimension_str(dimension: BreachDimension) -> &'static str {
    match dimension {
        BreachDimension::Tokens => "tokens",
        BreachDimension::Dollars => "dollars",
    }
}

fn scope_str(scope: BreachScope) -> &'static str {
    match scope {
        BreachScope::PerRun => "per-run",
        BreachScope::Cumulative => "cumulative",
    }
}

fn action_str(action: BreachAction) -> &'static str {
    match action {
        BreachAction::Pause => "pause",
        BreachAction::Stop => "stop",
        BreachAction::Warn => "warn",
    }
}

fn estimate_str(label: EstimateLabel) -> &'static str {
    label.as_str()
}

// ---------------------------------------------------------------------------
// The serde wire structs — the opentelemetry-proto JSON mapping, verbatim.
// ---------------------------------------------------------------------------

/// The POST body: one `ExportMetricsServiceRequest`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportMetricsServiceRequest {
    /// `resourceMetrics` — one resource (this host) per batch.
    pub resource_metrics: Vec<ResourceMetrics>,
}

/// `resourceMetrics` — the resource plus its scope metrics.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceMetrics {
    /// The exporting process's identity attributes.
    pub resource: Resource,
    /// The instrumentation scopes and their metrics.
    pub scope_metrics: Vec<ScopeMetrics>,
}

/// `resource` — `service.name=hekma-host`, the package version, and the
/// state-dir-derived instance id. Nothing else: minimal by AD-21.
#[derive(Debug, Serialize)]
pub struct Resource {
    /// `attributes` — all `stringValue` form.
    pub attributes: VecKeyValue,
}

/// `scopeMetrics` — the versioned exporter scope and its metric families.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeMetrics {
    /// The instrumentation scope identity.
    pub scope: InstrumentationScope,
    /// The metric families in this batch.
    pub metrics: Vec<Metric>,
}

/// `scope` — name + version (`otlp-metrics v1` rides here AND in the docs).
#[derive(Debug, Serialize)]
pub struct InstrumentationScope {
    /// The scope name.
    pub name: String,
    /// The scope (exposition) version.
    pub version: String,
}

/// One metric family — exactly one of `sum` / `gauge` / `histogram` is set.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metric {
    /// The metric name (semconv first: `gen_ai.*`, else `hekma.*`).
    pub name: String,
    /// The human description (omitted when absent — all v1 families carry one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The UCUM-ish unit (`{token}`, `{breach}`, `USD`, `{state}`, `{event}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// The SUM family (cumulative temporality, numeric `1`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sum: Option<Sum>,
    /// The GAUGE family.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gauge: Option<Gauge>,
    /// The HISTOGRAM family (temporality omitted — the proto default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub histogram: Option<Histogram>,
}

/// `sum` — cumulative, monotonic where the quantity only grows.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sum {
    /// The cumulative data points.
    pub data_points: Vec<NumberDataPoint>,
    /// Numeric `1` = CUMULATIVE (never the string form).
    pub aggregation_temporality: i32,
    /// Whether the sum can only increase (all v1 sums: true).
    pub is_monotonic: bool,
}

/// `gauge` — instantaneous values (the agent-state gauge).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Gauge {
    /// The current-value data points.
    pub data_points: Vec<NumberDataPoint>,
}

/// `histogram` — fixed documented bounds; temporality omitted (the default).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Histogram {
    /// The cumulative histogram data points.
    pub data_points: Vec<HistogramDataPoint>,
    /// `None` = the field is OMITTED (the proto's default temporality).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregation_temporality: Option<i32>,
}

/// `numberDataPoint` — int points carry `asInt` (decimal STRING), double
/// points carry `asDouble` (JSON number); never both.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NumberDataPoint {
    /// The series' identifying attributes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attributes: VecKeyValue,
    /// The cumulative window's open — decimal-string nanos.
    pub start_time_unix_nano: String,
    /// The observation moment — decimal-string nanos.
    pub time_unix_nano: String,
    /// The integer value (decimal string — the fixed64/sfixed64 JSON rule).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_int: Option<String>,
    /// The double value (JSON number).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_double: Option<f64>,
}

/// `histogramDataPoint` — `count` and `bucketCounts` are decimal STRINGS
/// (fixed64); `sum`/`min`/`max`/`explicitBounds` are JSON numbers (double).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistogramDataPoint {
    /// The series' identifying attributes.
    pub attributes: VecKeyValue,
    /// The cumulative window's open — decimal-string nanos.
    pub start_time_unix_nano: String,
    /// The observation moment — decimal-string nanos.
    pub time_unix_nano: String,
    /// The observation count — decimal string (the fixed64 rule).
    pub count: String,
    /// The observation sum (JSON number).
    pub sum: f64,
    /// Per-bucket counts — decimal strings, bounds.len() + 1 entries.
    pub bucket_counts: Vec<String>,
    /// The fixed documented bounds (JSON numbers).
    pub explicit_bounds: Vec<f64>,
    /// The smallest observation (JSON number).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// The largest observation (JSON number).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

/// The `repeated KeyValue` attribute list (a type alias keeps every struct's
/// field signature readable).
pub type VecKeyValue = Vec<KeyValue>;

/// One `keyValue` attribute.
#[derive(Debug, Serialize)]
pub struct KeyValue {
    /// The attribute key.
    pub key: String,
    /// The attribute value (string form only in v1).
    pub value: AttrValue,
}

/// `AnyValue` — only the `stringValue` variant is produced in v1.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttrValue {
    /// The string value (omitted when the variant is not string — never the
    /// case in this module's output).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub string_value: Option<String>,
}

/// Shorthand for a string attribute.
fn attr(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: AttrValue {
            string_value: Some(value.to_string()),
        },
    }
}

/// The wall-clock nanos since the Unix epoch (saturating — a pre-epoch clock
/// clamps at 0; this is a timestamp, not a correctness value).
pub fn unix_nanos_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
