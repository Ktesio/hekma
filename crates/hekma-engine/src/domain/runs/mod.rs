//! Runs as first-class entities (story 21-2, spine AD-25).
//!
//! A **Run** is one unit of agent work with a stable run id, a Control Depth,
//! labels, budgets and a Run Status (PRD FR-49). A supervised Agent
//! Instance's process span is one Run — a Restart Policy relaunch stays inside
//! it — and a Host can open child Runs beneath it (FR-43, the old Task scope).
//!
//! This module is the **Run registry** of AD-25: it lives in the engine core
//! beside the [`Supervisor`](super::Supervisor), which keeps only process
//! handles. Every Run fact is durable in state.db and has one writer, the
//! store writer (AD-27): the operations in [`ops`] run as writer jobs and
//! return the [`RunEvent`]s they committed plus the snapshot entries they
//! changed. The registry takes no lock of its own, so the fixed lock order
//! `Registry → Run registry → Supervisor` holds trivially, and a Supervisor
//! transition carries its Run projection inside its own writer job.
//!
//! * [`ControlDepth`], [`RunStatus`], [`Anomaly`], [`Label`] — the wire
//!   vocabulary (kebab-case, AD-14).
//! * [`RunRecord`] / [`RunView`] — one Run's row, and the read the facade
//!   returns.
//! * [`RunEvent`] — the Run event family on the bus and in the outbox.
//! * [`verbs`] — the verb × Control Depth table (FR-49).
//! * [`snapshot`] — the immutable Run index the writer publishes (AD-25).

use serde::{Deserialize, Serialize};

use super::budget::BreachAction;
use super::cost::{EstimateLabel, Micros};
use super::lifecycle::LifecycleState;
use super::usage::RunId;
use crate::ports::StoreError;

pub(crate) mod labels;
pub(crate) mod ops;
pub(crate) mod policy;
pub mod selector;
pub(crate) mod snapshot;
pub mod verbs;

pub use labels::{validate_label_key, validate_label_value, LabelChange, LabelOutcome};
pub use selector::{Selector, SelectorScope, MAX_SELECTED_RUNS};
pub use verbs::{verb_cell, CellBehavior, RunVerb, VerbCell, RUN_VERBS};

/// The schema version stamped on every [`RunEvent`] (AD-14). Independent of
/// the transition, breach and usage event schemas; bumped only on an
/// incompatible change (adding a field is additive).
pub const RUN_EVENT_SCHEMA_VERSION: u32 = 1;

/// The deepest a Run tree may nest: the root is level 1 (PRD FR-49).
pub const MAX_RUN_NESTING: usize = 8;

/// The identity binding a Host-supplied external id resolves under
/// (`run_identities`, AD-25). Later stories add identity-header and announce
/// bindings beside it.
pub const HOST_IDENTITY_BINDING: &str = "host";

/// How far Hekma controls a Run — a strict ladder, each rung including the
/// one below (PRD Glossary). `supervised (no wire)` is depth `supervised`
/// with `wire_path: false` on every surface (AD-25).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlDepth {
    /// Known from its telemetry or announcement only.
    Observed,
    /// Its model traffic crosses the Metering Proxy.
    Wire,
    /// Wire, and it declared itself (a Host-opened child Run is at most this).
    Announced,
    /// Announced, and Hekma owns the process.
    Supervised,
}

impl ControlDepth {
    /// Every depth, shallowest first.
    pub const ALL: [ControlDepth; 4] = [
        ControlDepth::Observed,
        ControlDepth::Wire,
        ControlDepth::Announced,
        ControlDepth::Supervised,
    ];

    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            ControlDepth::Observed => "observed",
            ControlDepth::Wire => "wire",
            ControlDepth::Announced => "announced",
            ControlDepth::Supervised => "supervised",
        }
    }

    /// Parse the wire form.
    pub fn from_wire(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|depth| depth.as_str() == raw)
    }

    /// The human rendering, naming the no-wire case (`supervised (no wire)`).
    pub fn display_with_wire(&self, wire_path: bool) -> String {
        if *self == ControlDepth::Supervised && !wire_path {
            "supervised (no wire)".to_string()
        } else {
            self.as_str().to_string()
        }
    }
}

/// A Run's status (PRD Glossary). `silent` and `stale` belong to the wire
/// shape and are not produced before Leases and Nodes exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunStatus {
    /// Running normally.
    Active,
    /// Paused, by its own verb or a parent's cascade.
    Held,
    /// No sign of life within the silent window.
    Silent,
    /// Its Node is unreachable.
    Stale,
    /// Ended — bookkeeping only; ending never acts on a process.
    Ended,
}

impl RunStatus {
    const ALL: [RunStatus; 5] = [
        RunStatus::Active,
        RunStatus::Held,
        RunStatus::Silent,
        RunStatus::Stale,
        RunStatus::Ended,
    ];

    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Active => "active",
            RunStatus::Held => "held",
            RunStatus::Silent => "silent",
            RunStatus::Stale => "stale",
            RunStatus::Ended => "ended",
        }
    }

    /// Parse the wire form.
    pub fn from_wire(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|status| status.as_str() == raw)
    }

    /// The Run Status a supervised Run shows for a Lifecycle State (spine
    /// AD-15 amendment). `failed` is decided by the caller, which knows
    /// whether the Restart Policy relaunches (`active` + the `failed`
    /// Anomaly) or not (`ended` + the Anomaly); `registered` has no Run.
    pub fn projected_from(state: LifecycleState) -> Option<Self> {
        match state {
            LifecycleState::Starting | LifecycleState::Running | LifecycleState::Stopping => {
                Some(RunStatus::Active)
            }
            LifecycleState::Paused => Some(RunStatus::Held),
            LifecycleState::Stopped => Some(RunStatus::Ended),
            LifecycleState::Registered | LifecycleState::Failed => None,
        }
    }
}

/// A condition Hekma makes loud (PRD Glossary). An Anomaly never acts on a
/// Run by itself. Story 21-2 stores these three; later stories add the
/// unscented, bypass, silent and policy-fallback kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Anomaly {
    /// A Token Budget or Cost Cap breached on this Run, under any Breach
    /// Action. Cleared only when the latch re-arms.
    Breach,
    /// The supervised process failed: set at the crash, cleared when a
    /// relaunch reaches `running`, kept when the Run ends.
    Failed,
    /// A Breach Action set on the Run cannot execute at its current depth
    /// and acts as `warn`; cleared when the depth supports it again.
    DepthFallback,
}

impl Anomaly {
    const ALL: [Anomaly; 3] = [Anomaly::Breach, Anomaly::Failed, Anomaly::DepthFallback];

    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Anomaly::Breach => "breach",
            Anomaly::Failed => "failed",
            Anomaly::DepthFallback => "depth-fallback",
        }
    }

    /// Parse the wire form.
    pub fn from_wire(raw: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|anomaly| anomaly.as_str() == raw)
    }

    /// Encode a set as the stored comma list (sorted, deduplicated; `""`
    /// for none).
    pub(crate) fn encode(set: &[Anomaly]) -> String {
        let mut sorted = set.to_vec();
        sorted.sort();
        sorted.dedup();
        sorted
            .iter()
            .map(Anomaly::as_str)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Decode the stored comma list; an unknown name is skipped (a newer
    /// engine's kind read by this one — never a failed read).
    pub(crate) fn decode(raw: &str) -> Vec<Anomaly> {
        let mut set: Vec<Anomaly> = raw.split(',').filter_map(Anomaly::from_wire).collect();
        set.sort();
        set.dedup();
        set
    }
}

/// Where a label came from (PRD FR-49): `bound` by the Operator or Host
/// (configuration, a Run Key, an endpoint, the label verb), or `caller`
/// (set at announcement or in identity headers). Only bound labels select
/// Policies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LabelSource {
    /// Set by the Operator or Host.
    Bound,
    /// Set by the workload itself.
    Caller,
}

impl LabelSource {
    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            LabelSource::Bound => "bound",
            LabelSource::Caller => "caller",
        }
    }

    /// Parse the wire form.
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw {
            "bound" => Some(LabelSource::Bound),
            "caller" => Some(LabelSource::Caller),
            _ => None,
        }
    }
}

/// Where a bound label came from (story 21-9). Per key, `label-verb` wins
/// over `config`, and any bound label wins over a Caller's (FR-49): a
/// configuration re-sync replaces only `config` rows, and the label verb never
/// rewrites the configuration. Later sources join beside these (a Run Key, an
/// endpoint binding).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LabelOrigin {
    /// The instance's `labels.<key>` configuration.
    Config,
    /// The Operator's label verb (`hekma label`, `PATCH /v0/runs/{id}/labels`).
    LabelVerb,
}

impl LabelOrigin {
    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            LabelOrigin::Config => "config",
            LabelOrigin::LabelVerb => "label-verb",
        }
    }

    /// Parse the wire form.
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw {
            "config" => Some(LabelOrigin::Config),
            "label-verb" => Some(LabelOrigin::LabelVerb),
            _ => None,
        }
    }
}

/// One `key=value` label with its source (and, for a bound label, its
/// origin).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Label {
    /// The key (see [`validate_label_key`]).
    pub key: String,
    /// The value (see [`validate_label_value`]).
    pub value: String,
    /// Who set it.
    pub source: LabelSource,
    /// Where a bound label came from; `None` on a Caller label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<LabelOrigin>,
}

impl Label {
    /// A bound label from the instance configuration (origin `config`).
    pub fn bound(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self::bound_by(key, value, LabelOrigin::Config)
    }

    /// A bound label set by the label verb (origin `label-verb`).
    pub fn verb(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self::bound_by(key, value, LabelOrigin::LabelVerb)
    }

    /// A bound label with an explicit origin.
    pub fn bound_by(key: impl Into<String>, value: impl Into<String>, origin: LabelOrigin) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            source: LabelSource::Bound,
            origin: Some(origin),
        }
    }

    /// A caller label.
    pub fn caller(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            source: LabelSource::Caller,
            origin: None,
        }
    }

    /// Whether this label is bound.
    pub fn is_bound(&self) -> bool {
        self.source == LabelSource::Bound
    }

    /// A bound label's effective origin: a bound row with no recorded origin
    /// can only have come from configuration (the one bound source before
    /// origins were recorded). `None` on a Caller label.
    pub fn bound_origin(&self) -> Option<LabelOrigin> {
        self.is_bound()
            .then(|| self.origin.unwrap_or(LabelOrigin::Config))
    }
}

/// The limits set directly on a Run (story 21-2: on a child Run at open).
/// A supervised Run's ceilings come from its instance configuration, which
/// the pipeline resolves; these are the Run-level ones (`run_limits`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLimits {
    /// A per-run token ceiling over the Run's subtree usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_per_run: Option<u64>,
    /// A per-run dollar cap over the Run's subtree cost, in micro-dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dollar_per_run_micros: Option<Micros>,
    /// The Breach Action for these ceilings (`warn` when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breach_action: Option<BreachAction>,
}

impl RunLimits {
    /// Whether any ceiling is set.
    pub fn is_armed(&self) -> bool {
        self.token_per_run.is_some() || self.dollar_per_run_micros.is_some()
    }

    /// The Breach Action these ceilings execute (`warn` when unset).
    pub fn action(&self) -> BreachAction {
        self.breach_action.unwrap_or(BreachAction::Warn)
    }
}

/// One Run's stored row (AD-25).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    /// The stable run id.
    pub run_id: RunId,
    /// Its Control Depth.
    pub control_depth: ControlDepth,
    /// Whether its model traffic crosses the Metering Proxy.
    pub wire_path: bool,
    /// Its Run Status.
    pub status: RunStatus,
    /// Its Anomaly flags (sorted).
    pub anomalies: Vec<Anomaly>,
    /// The parent Run, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<RunId>,
    /// The hosting Agent Instance (its own for a supervised Run, the host
    /// process's for an in-process child).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Whether this Run owns the instance's supervised process.
    pub process_owner: bool,
    /// Why it ended, once ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_cause: Option<String>,
    /// RFC 3339 creation time.
    pub created_at: String,
    /// RFC 3339 time of the last change.
    pub updated_at: String,
    /// RFC 3339 end time, once ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
}

impl RunRecord {
    /// Whether the Run has ended.
    pub fn is_ended(&self) -> bool {
        self.status == RunStatus::Ended
    }
}

/// A Run's usage over its subtree (its own usage plus every descendant's),
/// from the rollups (AD-27).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunUsage {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// The cached subset of the input, when every row reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// The derived cost in integer micro-dollars (zero when unpriced).
    pub cost_micros: Micros,
    /// Always `estimated` until reconciliation lands (AD-8).
    pub estimate: EstimateLabel,
}

/// What the facade returns for one Run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunView {
    /// The stored row.
    #[serde(flatten)]
    pub record: RunRecord,
    /// Its labels, bound and caller, sorted by key.
    pub labels: Vec<Label>,
    /// Limits set directly on the Run.
    pub limits: RunLimits,
    /// Its subtree usage.
    pub usage: RunUsage,
    /// How many of its children have not ended.
    pub open_children: u64,
}

/// Where a child Run is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunParent {
    /// Under this Run.
    Run(RunId),
    /// Under this instance's open supervised Run.
    Instance(String),
}

/// A request to open a child Run (FR-43, FR-49).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildRunRequest {
    /// The parent.
    pub parent: RunParent,
    /// Caller labels (`key`, `value`).
    pub labels: Vec<(String, String)>,
    /// Ceilings on the child's subtree.
    pub limits: RunLimits,
    /// An external id under which a retried open returns the same Run.
    pub external_id: Option<String>,
}

impl ChildRunRequest {
    /// A bare request under `parent`.
    pub fn under(parent: RunParent) -> Self {
        Self {
            parent,
            labels: Vec::new(),
            limits: RunLimits::default(),
            external_id: None,
        }
    }
}

/// What happened to a Run (the `change` of a [`RunEvent`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunEventKind {
    /// The Run was created.
    Created,
    /// Its Control Depth or wire path changed (the id is kept).
    DepthChanged,
    /// Its Run Status changed (not to `ended`).
    StatusChanged,
    /// Its Anomaly flags changed with no status change.
    AnomaliesChanged,
    /// Its bound labels changed.
    LabelsChanged,
    /// It ended.
    Ended,
}

impl RunEventKind {
    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            RunEventKind::Created => "created",
            RunEventKind::DepthChanged => "depth-changed",
            RunEventKind::StatusChanged => "status-changed",
            RunEventKind::AnomaliesChanged => "anomalies-changed",
            RunEventKind::LabelsChanged => "labels-changed",
            RunEventKind::Ended => "ended",
        }
    }
}

/// One Run event (AD-14 amendment 2026-10-30): the Run family on the bus and
/// in the outbox. It carries the Run's state after the change, so a consumer
/// needs no read to render it; Policy names join with Policies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvent {
    /// [`RUN_EVENT_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// What happened (`change`, since the bus wrapper's tag is `kind`).
    pub change: RunEventKind,
    /// The Run.
    pub run_id: RunId,
    /// Its parent, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<RunId>,
    /// Its hosting instance, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Its Control Depth after the change.
    pub control_depth: ControlDepth,
    /// Its wire path after the change.
    pub wire_path: bool,
    /// Its Run Status after the change.
    pub status: RunStatus,
    /// The status before, on `status-changed` and `ended`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_status: Option<RunStatus>,
    /// The depth before, on `depth-changed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_control_depth: Option<ControlDepth>,
    /// The wire path before, on `depth-changed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_wire_path: Option<bool>,
    /// Its Anomaly flags after the change.
    pub anomalies: Vec<Anomaly>,
    /// Its bound labels after the change.
    pub labels: Vec<Label>,
    /// Its bound labels before the change, on `labels-changed` (story 21-9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_labels: Option<Vec<Label>>,
    /// Why (a transition cause, `parent-paused`, `closed`, `label-verb`,
    /// `config`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// RFC 3339 time of the change.
    pub at: String,
}

/// Why a Run operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// No Run has this id.
    #[error("no Run has the id `{run_id}`")]
    NotFound {
        /// The id asked for.
        run_id: String,
    },
    /// The instance has no open supervised Run to open a child under.
    #[error("Agent Instance `{instance}` has no open Run — start it first")]
    NoOpenRun {
        /// The instance asked for.
        instance: String,
    },
    /// No Agent Instance has this name (story 21-9: an agent named as a
    /// label target).
    #[error("no Agent Instance is named `{instance}`")]
    InstanceNotFound {
        /// The name asked for.
        instance: String,
    },
    /// The parent has ended.
    #[error("Run `{run_id}` has ended, so a child Run cannot be opened under it")]
    ParentEnded {
        /// The parent.
        run_id: String,
    },
    /// The child would nest deeper than [`MAX_RUN_NESTING`].
    #[error("a child Run under `{parent}` would nest {depth} levels deep (at most {max})")]
    NestingTooDeep {
        /// The parent.
        parent: String,
        /// The level the child would sit at.
        depth: usize,
        /// [`MAX_RUN_NESTING`].
        max: usize,
    },
    /// The Breach Action cannot execute at the Run's depth.
    #[error(
        "Breach Action `{action}` cannot be set on a {depth} Run (Metering Source \
         `{metering_source}`): {reason}"
    )]
    ActionUnavailable {
        /// The action asked for.
        action: String,
        /// The Run's depth, rendered (`announced`, …).
        depth: String,
        /// The hosting process's Metering Source.
        metering_source: String,
        /// Why.
        reason: String,
    },
    /// `close` on a Run that owns a supervised process.
    #[error(
        "Run `{run_id}` owns the supervised process of `{instance}` — stop the instance \
         instead of closing the Run"
    )]
    OwnsProcess {
        /// The Run.
        run_id: String,
        /// Its instance.
        instance: String,
    },
    /// The Run has ended, so its labels cannot change (story 21-9).
    #[error("Run `{run_id}` has ended, so its labels cannot change")]
    RunEnded {
        /// The Run.
        run_id: String,
    },
    /// The label verb cannot remove a label the instance configuration binds
    /// (story 21-9).
    #[error(
        "label `{key}` on Run `{run_id}` comes from the configuration key `labels.{key}` of \
         agent `{instance}`, so it cannot be removed from the Run — change it with \
         `hekma agent config set {instance} labels.{key} <value>`, or override it on the Run \
         with `hekma label set {run_id} {key}=<value>`"
    )]
    LabelFromConfig {
        /// The Run.
        run_id: String,
        /// The label key.
        key: String,
        /// The instance whose configuration binds it.
        instance: String,
    },
    /// The label verb removes bound labels only; this key is a Caller's.
    #[error(
        "label `{key}` on Run `{run_id}` was set by the Run's Caller, not bound, so the label \
         verb cannot remove it (it removes only labels it set)"
    )]
    LabelNotBound {
        /// The Run.
        run_id: String,
        /// The label key.
        key: String,
    },
    /// A Caller label write names a key already bound on the Run (FR-49).
    #[error("label `{key}` is bound on Run `{run_id}`, so a Caller cannot set it")]
    LabelBound {
        /// The Run.
        run_id: String,
        /// The label key.
        key: String,
    },
    /// A Selector matched more Runs than one request may act on.
    #[error(
        "Selector `{selector}` matches more than {max} Runs — narrow it with another \
         `key=value` term"
    )]
    TooManyRuns {
        /// The Selector.
        selector: String,
        /// [`MAX_SELECTED_RUNS`].
        max: usize,
    },
    /// A malformed request field.
    #[error("{field}: {reason}")]
    Invalid {
        /// The field (`label`, `token-budget`, …).
        field: String,
        /// Why.
        reason: String,
    },
    /// The store refused or failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl RunError {
    /// A stable machine code for wire surfaces.
    pub fn code(&self) -> &'static str {
        match self {
            RunError::NotFound { .. } => "run-not-found",
            RunError::NoOpenRun { .. } => "no-open-run",
            RunError::InstanceNotFound { .. } => "instance-not-found",
            RunError::ParentEnded { .. } => "parent-ended",
            RunError::NestingTooDeep { .. } => "nesting-too-deep",
            RunError::ActionUnavailable { .. } => "action-unavailable",
            RunError::OwnsProcess { .. } => "owns-process",
            RunError::RunEnded { .. } => "run-ended",
            RunError::LabelFromConfig { .. } => "label-from-config",
            RunError::LabelNotBound { .. } => "label-not-bound",
            RunError::LabelBound { .. } => "label-bound",
            RunError::TooManyRuns { .. } => "too-many-runs",
            RunError::Invalid { .. } => "invalid-request",
            RunError::Store(_) => "store",
        }
    }
}

#[cfg(test)]
mod tests;
