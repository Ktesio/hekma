//! The `otlp-metrics v2` exposition (story 24-5, FR-59, AD-21's 2026-10-30
//! amendment), and the state the export task drives — v2 by default, v1
//! (unchanged, deprecated) behind `--otlp-exposition v1`.
//!
//! v2 keys every series by what an Operator steers, never by Run:
//! * `hekma.run_id` is on NO data point's attributes; per-Run detail rides
//!   exemplars (at most the 8 most-recent distinct Runs per point, ONLY
//!   `hekma.run_id` in `filteredAttributes`, no trace or span id);
//! * attributes: `gen_ai.token.type`, `hekma.instance` (when the Run has
//!   one; the 256 most-recently-observed instances), `hekma.control_depth`
//!   (from the event), `hekma.policy` (limit-tied families only), and the
//!   Operator's allow-listed `hekma.label.<key>` (empty by default, capped
//!   per key with an overflow fold);
//! * Runs with no Agent Instance (wire and observed Runs) export too.
//!
//! New families: `hekma.run.anomalies`, `hekma.calls.unscented`,
//! `hekma.telemetry.received`, `hekma.bypass.detected` (story 22-4: by
//! `hekma.provider.host`, depth and allow-listed labels, the Run via
//! exemplar; only detections that are the `bypass` Anomaly — the expected,
//! informational ones are not bypasses) and the self-metric
//! `hekma.otlp.folded`.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use hekma_engine::{BreachDimension, BreachScope, EngineEvent, EstimateLabel, ExportCounters};

use super::attributes::{depth_str, InstanceWindow, LabelProjection, INSTANCE_WINDOW};
use super::payload::{
    attr, unix_nanos_now, Exemplar, ExportMetricsServiceRequest, Gauge, Histogram,
    HistogramAccumulator, HistogramDataPoint, InstrumentationScope, Metric, MetricState,
    NumberDataPoint, Resource, ResourceMetrics, ScopeMetrics, Sum, TokenType,
    AGGREGATION_TEMPORALITY_CUMULATIVE, SCOPE_NAME, TOKEN_HISTOGRAM_BOUNDS,
};
use super::Exposition;

/// The exposition version this module emits.
pub const EXPOSITION_VERSION_V2: &str = "v2";

/// The most exemplars one data point carries (the most recent distinct
/// Runs).
pub const MAX_EXEMPLARS: usize = 8;

/// One series' attributes, in their emitted order (the map key).
type Attrs = Vec<(String, String)>;

/// The most-recent distinct Runs observed on one series.
#[derive(Clone, Debug, Default)]
struct Reservoir(VecDeque<(String, u64, u64)>);

impl Reservoir {
    /// Record `run` observed at `time` with `value`: it moves to the front,
    /// and only the [`MAX_EXEMPLARS`] most recent distinct Runs stay.
    fn observe(&mut self, run: &str, time: u64, value: u64) {
        self.0.retain(|(seen, _, _)| seen != run);
        self.0.push_front((run.to_string(), time, value));
        self.0.truncate(MAX_EXEMPLARS);
    }

    fn exemplars(&self) -> Vec<Exemplar> {
        self.0
            .iter()
            .map(|(run, time, value)| Exemplar {
                filtered_attributes: vec![attr("hekma.run_id", run)],
                time_unix_nano: time.to_string(),
                as_int: Some(value.to_string()),
                as_double: None,
            })
            .collect()
    }
}

/// The v2 cumulative metric state (since process start — the restart-reset
/// holds, as in v1).
#[derive(Clone, Debug)]
pub struct MetricStateV2 {
    start_time_unix_nano: u64,
    service_instance_id: String,
    labels: LabelProjection,
    window: InstanceWindow,
    token_usage: BTreeMap<Attrs, (HistogramAccumulator, Reservoir)>,
    breaches: BTreeMap<Attrs, (u64, Reservoir)>,
    bypass: BTreeMap<Attrs, (u64, Reservoir)>,
    cost: BTreeMap<Attrs, f64>,
    agent_state: BTreeMap<(String, String), i64>,
    anomalies: BTreeMap<Attrs, u64>,
    /// Each open Run's Anomaly flags, to count a flag once when gained.
    /// A Run leaves when it ends.
    run_anomalies: HashMap<String, BTreeSet<String>>,
    counters: ExportCounters,
    dropped: u64,
    in_flight: u64,
}

impl MetricStateV2 {
    /// Fresh state with the label allow-list and its per-key cap.
    pub fn new(
        start_time_unix_nano: u64,
        service_instance_id: String,
        label_keys: Vec<String>,
        label_cardinality: u32,
    ) -> Self {
        MetricStateV2 {
            start_time_unix_nano,
            service_instance_id,
            labels: LabelProjection::new(label_keys, label_cardinality),
            window: InstanceWindow::default(),
            token_usage: BTreeMap::new(),
            breaches: BTreeMap::new(),
            bypass: BTreeMap::new(),
            cost: BTreeMap::new(),
            agent_state: BTreeMap::new(),
            anomalies: BTreeMap::new(),
            run_anomalies: HashMap::new(),
            counters: ExportCounters::default(),
            dropped: 0,
            in_flight: 0,
        }
    }

    /// Mark `instance` observed, and drop the series of instances that
    /// leave the window.
    fn touch_instance(&mut self, instance: &str) {
        for gone in self.window.touch(instance) {
            let carries = |attrs: &Attrs| {
                attrs
                    .iter()
                    .any(|(key, value)| key == "hekma.instance" && *value == gone)
            };
            self.token_usage.retain(|attrs, _| !carries(attrs));
            self.breaches.retain(|attrs, _| !carries(attrs));
            self.cost.retain(|attrs, _| !carries(attrs));
            self.agent_state.retain(|(name, _), _| *name != gone);
        }
    }

    /// The instance and depth attributes a Run's event carries.
    fn base(&mut self, instance: Option<&str>, depth: Option<hekma_engine::ControlDepth>) -> Attrs {
        let mut attrs = Vec::new();
        if let Some(instance) = instance {
            self.touch_instance(instance);
            attrs.push(("hekma.instance".to_string(), instance.to_string()));
        }
        if let Some(depth) = depth {
            attrs.push((
                "hekma.control_depth".to_string(),
                depth_str(depth).to_string(),
            ));
        }
        attrs
    }

    /// Fold one committed engine event, its exemplars stamped `time`.
    pub fn observe_at(&mut self, event: &EngineEvent, time: u64) {
        self.in_flight = self.in_flight.saturating_add(1);
        match event {
            EngineEvent::UsageUpdate(update) => {
                let usage = &update.event;
                let base = self.base(usage.instance.as_deref(), update.control_depth);
                let labels = self.labels.project(&update.labels);
                let run = usage.run_id.as_str().to_string();
                for (token_type, value) in [
                    (TokenType::Input, Some(usage.input_tokens)),
                    (TokenType::Output, Some(usage.output_tokens)),
                    (TokenType::Cached, usage.cached_tokens),
                ] {
                    let Some(tokens) = value else { continue };
                    let mut attrs = vec![(
                        "gen_ai.token.type".to_string(),
                        token_type.as_str().to_string(),
                    )];
                    attrs.extend(base.iter().cloned());
                    attrs.extend(labels.iter().cloned());
                    let (histogram, reservoir) = self.token_usage.entry(attrs).or_default();
                    histogram.observe(tokens as f64);
                    reservoir.observe(&run, time, tokens);
                }
            }
            EngineEvent::BudgetBreach(breach) => {
                let base = self.base(breach.instance.as_deref(), breach.control_depth);
                let labels = self.labels.project(&breach.labels);
                let policy = breach
                    .policy
                    .clone()
                    .unwrap_or_else(|| "default".to_string());
                let mut attrs = base.clone();
                attrs.push(("hekma.policy".to_string(), policy));
                attrs.push((
                    "hekma.budget.dimension".to_string(),
                    match breach.dimension {
                        BreachDimension::Tokens => "tokens",
                        BreachDimension::Dollars => "dollars",
                    }
                    .to_string(),
                ));
                attrs.push((
                    "hekma.budget.scope".to_string(),
                    match breach.scope {
                        BreachScope::PerRun => "per-run",
                        BreachScope::Cumulative => "cumulative",
                    }
                    .to_string(),
                ));
                attrs.push((
                    "hekma.budget.action".to_string(),
                    breach.action.as_str().to_string(),
                ));
                attrs.extend(labels.iter().cloned());
                let (count, reservoir) = self.breaches.entry(attrs).or_default();
                *count = count.saturating_add(1);
                reservoir.observe(&breach.run_id, time, 1);
                // The cost series, as in v1: a cumulative dollar breach with
                // an observed total, its estimate label MANDATORY.
                if breach.dimension == BreachDimension::Dollars
                    && breach.scope == BreachScope::Cumulative
                {
                    if let Some(micros) = breach.dollar_observed {
                        let label = breach
                            .estimate_label
                            .unwrap_or(EstimateLabel::Estimated)
                            .as_str();
                        let mut attrs = base;
                        attrs.push(("hekma.estimate_state".to_string(), label.to_string()));
                        attrs.extend(labels);
                        let usd = micros.get().max(0) as f64 / 1_000_000.0;
                        let entry = self.cost.entry(attrs).or_insert(0.0);
                        *entry = entry.max(usd);
                    }
                }
            }
            EngineEvent::Transition(transition) => {
                self.touch_instance(&transition.instance);
                self.agent_state.insert(
                    (
                        transition.instance.clone(),
                        transition.prior_state.as_str().to_string(),
                    ),
                    0,
                );
                self.agent_state.insert(
                    (
                        transition.instance.clone(),
                        transition.new_state.as_str().to_string(),
                    ),
                    1,
                );
            }
            EngineEvent::Run(run) => {
                let id = run.run_id.as_str().to_string();
                // Story 22-4: a detection that is the Anomaly counts once
                // per new host (the event's cause names it).
                if run.change == hekma_engine::RunEventKind::BypassDetected {
                    if let Some(host) = run
                        .cause
                        .as_deref()
                        .filter(|cause| !cause.contains("(informational"))
                    {
                        let mut attrs = vec![
                            ("hekma.provider.host".to_string(), host.to_string()),
                            (
                                "hekma.control_depth".to_string(),
                                depth_str(run.control_depth).to_string(),
                            ),
                        ];
                        let labels: Vec<(String, String)> = run
                            .labels
                            .iter()
                            .map(|label| (label.key.clone(), label.value.clone()))
                            .collect();
                        attrs.extend(self.labels.project(&labels));
                        let (count, reservoir) = self.bypass.entry(attrs).or_default();
                        *count = count.saturating_add(1);
                        reservoir.observe(&id, time, 1);
                    }
                }
                let now: BTreeSet<String> = run
                    .anomalies
                    .iter()
                    .map(|anomaly| anomaly.as_str().to_string())
                    .collect();
                let before = self.run_anomalies.get(&id).cloned().unwrap_or_default();
                for gained in now.difference(&before) {
                    let attrs = vec![
                        ("hekma.anomaly".to_string(), gained.clone()),
                        (
                            "hekma.control_depth".to_string(),
                            depth_str(run.control_depth).to_string(),
                        ),
                    ];
                    let count = self.anomalies.entry(attrs).or_insert(0);
                    *count = count.saturating_add(1);
                }
                if run.change == hekma_engine::RunEventKind::Ended || now.is_empty() {
                    self.run_anomalies.remove(&id);
                } else {
                    self.run_anomalies.insert(id, now);
                }
            }
            // No series: Policy changes, the default upgrade, credentials,
            // bindings, reveals, proxy settings.
            EngineEvent::Policy(_)
            | EngineEvent::BreachDefaultUpgraded(_)
            | EngineEvent::Credential(_)
            | EngineEvent::Binding(_)
            | EngineEvent::SecretRevealed(_)
            | EngineEvent::ProxySetting(_) => {}
        }
    }

    /// Take the engine's counters as of this flush.
    pub fn sample(&mut self, counters: ExportCounters) {
        self.counters = counters;
    }

    /// The once-per-flush notes: folded label keys and evicted instances.
    pub fn take_notes(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        for key in self.labels.take_folded_keys() {
            notes.push(format!(
                "otlp export: label key {key:?} reached its distinct-value cap; further \
                 values are exported as {:?} (counted on hekma.otlp.folded)",
                super::attributes::OVERFLOW_VALUE
            ));
        }
        let evicted = self.window.take_evicted();
        if evicted > 0 {
            notes.push(format!(
                "otlp export: {evicted} Agent Instance(s) left the {INSTANCE_WINDOW}-instance \
                 window; their series are no longer exported (a bounded window, not a drop)"
            ));
        }
        notes
    }

    /// Build the request body at `time_unix_nano` (pure).
    pub fn snapshot(&self, time_unix_nano: u64) -> ExportMetricsServiceRequest {
        let start = self.start_time_unix_nano.to_string();
        let now = time_unix_nano.max(self.start_time_unix_nano).to_string();
        let kv = |attrs: &Attrs| {
            attrs
                .iter()
                .map(|(key, value)| attr(key, value))
                .collect::<Vec<_>>()
        };
        let point = |attrs: Vec<_>, int: u64| NumberDataPoint {
            attributes: attrs,
            start_time_unix_nano: start.clone(),
            time_unix_nano: now.clone(),
            as_int: Some(int.to_string()),
            as_double: None,
            exemplars: Vec::new(),
        };
        let counter = |name: &str, description: &str, unit: &str, points| Metric {
            name: name.to_string(),
            description: Some(description.to_string()),
            unit: Some(unit.to_string()),
            sum: Some(Sum {
                data_points: points,
                aggregation_temporality: AGGREGATION_TEMPORALITY_CUMULATIVE,
                is_monotonic: true,
            }),
            gauge: None,
            histogram: None,
        };
        let mut metrics = Vec::new();
        if !self.token_usage.is_empty() {
            let data_points = self
                .token_usage
                .iter()
                .map(|(attrs, (accum, reservoir))| HistogramDataPoint {
                    attributes: kv(attrs),
                    start_time_unix_nano: start.clone(),
                    time_unix_nano: now.clone(),
                    count: accum.count.to_string(),
                    sum: accum.sum,
                    bucket_counts: accum.buckets.iter().map(|c| c.to_string()).collect(),
                    explicit_bounds: TOKEN_HISTOGRAM_BOUNDS.to_vec(),
                    min: Some(accum.min),
                    max: Some(accum.max),
                    exemplars: reservoir.exemplars(),
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
                    aggregation_temporality: None,
                }),
            });
        }
        if !self.breaches.is_empty() {
            let points = self
                .breaches
                .iter()
                .map(|(attrs, (count, reservoir))| NumberDataPoint {
                    exemplars: reservoir.exemplars(),
                    ..point(kv(attrs), *count)
                })
                .collect();
            metrics.push(counter(
                "hekma.budget.breaches",
                "Recorded budget breaches, regardless of the action taken",
                "{breach}",
                points,
            ));
        }
        if !self.bypass.is_empty() {
            let points = self
                .bypass
                .iter()
                .map(|(attrs, (count, reservoir))| NumberDataPoint {
                    exemplars: reservoir.exemplars(),
                    ..point(kv(attrs), *count)
                })
                .collect();
            metrics.push(counter(
                "hekma.bypass.detected",
                "Provider hosts a Run's process tree reached directly while its calls go \
                 through Hekma with key custody on (a signal, never an action)",
                "{detection}",
                points,
            ));
        }
        if !self.cost.is_empty() {
            let points = self
                .cost
                .iter()
                .map(|(attrs, usd)| NumberDataPoint {
                    as_int: None,
                    as_double: Some(*usd),
                    ..point(kv(attrs), 0)
                })
                .collect();
            metrics.push(counter(
                "hekma.usage.cost",
                "Derived cumulative cost, labeled estimated or reconciled — never an \
                 unlabeled dollar",
                "USD",
                points,
            ));
        }
        if !self.agent_state.is_empty() {
            let data_points = self
                .agent_state
                .iter()
                .map(|((instance, state), value)| NumberDataPoint {
                    as_int: Some(value.to_string()),
                    ..point(
                        vec![attr("hekma.instance", instance), attr("hekma.state", state)],
                        0,
                    )
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
        if !self.anomalies.is_empty() {
            let points = self
                .anomalies
                .iter()
                .map(|(attrs, count)| point(kv(attrs), *count))
                .collect();
            metrics.push(counter(
                "hekma.run.anomalies",
                "Anomaly flags gained by Runs, by kind and Control Depth",
                "{anomaly}",
                points,
            ));
        }
        let c = self.counters;
        metrics.push(counter(
            "hekma.calls.unscented",
            "Calls no Hekma credential identified: served (shadow) or refused (enforce)",
            "{call}",
            vec![
                point(vec![attr("outcome", "served")], c.unscented_served),
                point(vec![attr("outcome", "refused")], c.unscented_refused),
            ],
        ));
        metrics.push(counter(
            "hekma.telemetry.received",
            "Inbound telemetry points, accepted or skipped",
            "{point}",
            vec![
                point(vec![attr("outcome", "accepted")], c.telemetry_accepted),
                point(vec![attr("outcome", "skipped")], c.telemetry_skipped),
            ],
        ));
        metrics.push(counter(
            "hekma.otlp.folded",
            "Label values folded into the overflow value past their key's cap",
            "{observation}",
            vec![point(Vec::new(), self.labels.folded())],
        ));
        metrics.push(counter(
            "hekma.otlp.dropped",
            "Exporter losses: bus lag, queue overflow, dropped batches, \
             collector-rejected points",
            "{event}",
            vec![point(Vec::new(), self.dropped)],
        ));
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
                        version: EXPOSITION_VERSION_V2.to_string(),
                    },
                    metrics,
                }],
            }],
        }
    }
}

/// The state the export task drives: v1 or v2 (story 24-5).
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ExporterState {
    /// `otlp-metrics v1` (deprecated).
    V1(MetricState),
    /// `otlp-metrics v2` (the default).
    V2(MetricStateV2),
}

impl ExporterState {
    /// Fresh state for `exposition`.
    pub fn new(
        exposition: Exposition,
        start_time_unix_nano: u64,
        service_instance_id: String,
        label_keys: Vec<String>,
        label_cardinality: u32,
    ) -> Self {
        match exposition {
            Exposition::V1 => {
                ExporterState::V1(MetricState::new(start_time_unix_nano, service_instance_id))
            }
            Exposition::V2 => ExporterState::V2(MetricStateV2::new(
                start_time_unix_nano,
                service_instance_id,
                label_keys,
                label_cardinality,
            )),
        }
    }

    /// The exposition version word.
    pub fn version(&self) -> &'static str {
        match self {
            ExporterState::V1(_) => super::payload::EXPOSITION_VERSION,
            ExporterState::V2(_) => EXPOSITION_VERSION_V2,
        }
    }

    pub fn observe(&mut self, event: &EngineEvent) {
        match self {
            ExporterState::V1(state) => state.observe(event),
            ExporterState::V2(state) => state.observe_at(event, unix_nanos_now()),
        }
    }

    /// Take the engine's counters (v2 only; v1 has no such families).
    pub fn sample(&mut self, counters: ExportCounters) {
        if let ExporterState::V2(state) = self {
            state.sample(counters);
        }
    }

    /// The once-per-flush notes.
    pub fn take_notes(&mut self) -> Vec<String> {
        match self {
            ExporterState::V1(state) => state
                .take_evicted_run_series()
                .into_iter()
                .map(|(instance, count)| {
                    format!(
                        "otlp export: the token-usage run window for {instance} is full — \
                         {count} run series beyond the {}-run bound were evicted from the \
                         live payload (the durable ledger keeps every run; this is a bounded \
                         window, not a drop)",
                        super::payload::MAX_RUNS_PER_INSTANCE
                    )
                })
                .collect(),
            ExporterState::V2(state) => state.take_notes(),
        }
    }

    pub fn snapshot(&self, time_unix_nano: u64) -> ExportMetricsServiceRequest {
        match self {
            ExporterState::V1(state) => state.snapshot(time_unix_nano),
            ExporterState::V2(state) => state.snapshot(time_unix_nano),
        }
    }

    pub fn note_dropped_events(&mut self, n: u64) {
        match self {
            ExporterState::V1(state) => state.note_dropped_events(n),
            ExporterState::V2(state) => state.dropped = state.dropped.saturating_add(n),
        }
    }

    pub fn note_dropped_batch(&mut self) {
        match self {
            ExporterState::V1(state) => state.note_dropped_batch(),
            ExporterState::V2(state) => state.dropped = state.dropped.saturating_add(1),
        }
    }

    pub fn note_rejected_points(&mut self, n: u64) {
        match self {
            ExporterState::V1(state) => state.note_rejected_points(n),
            ExporterState::V2(state) => state.dropped = state.dropped.saturating_add(n),
        }
    }

    pub fn in_flight(&self) -> u64 {
        match self {
            ExporterState::V1(state) => state.in_flight(),
            ExporterState::V2(state) => state.in_flight,
        }
    }

    pub fn mark_delivered(&mut self) {
        match self {
            ExporterState::V1(state) => state.mark_delivered(),
            ExporterState::V2(state) => state.in_flight = 0,
        }
    }

    pub fn dropped(&self) -> u64 {
        match self {
            ExporterState::V1(state) => state.dropped(),
            ExporterState::V2(state) => state.dropped,
        }
    }
}

#[cfg(test)]
mod tests;
