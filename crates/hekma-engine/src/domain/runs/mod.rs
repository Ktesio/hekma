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

pub mod announce;
pub mod dispositions;
pub mod bindings;
pub(crate) mod keys;
pub(crate) mod labels;
pub(crate) mod lease;
pub(crate) mod limits;
pub(crate) mod listing;
pub(crate) mod ops;
pub(crate) mod policy;
pub mod selector;
pub(crate) mod snapshot;
pub mod telemetry;
pub mod verbs;
pub(crate) mod wire;

pub use labels::{validate_label_key, validate_label_value, LabelChange, LabelOutcome};
pub use limits::{
    LimitActor, LimitsDirection, RunLimitChange, RunLimitField, RunLimitsOutcome, RunLimitsResults,
    RunLimitsView,
};
pub use listing::{
    decode_cursor, encode_cursor, ended_run_cutoff, CustodyView, LatchedBreach, ListRunsQuery,
    ListingCounts, ListingView, PerfRunRow, RunBudgetView, RunListing, RunListingDocument,
    RunSummary, ENDED_RUN_RETENTION_DAYS, LISTING_DEFAULT_LIMIT, LISTING_MAX_LIMIT,
    RUN_LISTING_SCHEMA_VERSION,
};
pub use policy::{
    BreachDefaultUpgradedEvent, LimitKind, LimitSource, LimitSources, PolicyEvent, PolicyEventKind,
    PolicyLimits, PolicyRecord, PolicyResolution, ResolveInputs, RunPolicy, ScentCheckMode,
    UpgradedInstance, DEFAULT_POLICY_NAME, POLICY_EVENT_SCHEMA_VERSION,
};
pub use selector::{Selector, SelectorScope, MAX_SELECTED_RUNS};
pub use verbs::{verb_cell, CellBehavior, RunLifecycleVerb, RunVerb, VerbCell, RUN_VERBS};

/// The schema version stamped on every [`RunEvent`] (AD-14). Independent of
/// the transition, breach and usage event schemas; bumped only on an
/// incompatible change (adding a field is additive).
pub const RUN_EVENT_SCHEMA_VERSION: u32 = 1;

/// The deepest a Run tree may nest: the root is level 1 (PRD FR-49).
pub const MAX_RUN_NESTING: usize = 8;

/// The most open children one parent may have when a Caller opens them with
/// a Run Key (story 22-1, threat T1.9: a Run Key can create Runs). The
/// Operator and Host are not capped.
pub const MAX_CALLER_OPEN_CHILDREN: u64 = 1000;

/// The most open Runs one root's subtree may hold when Callers open or
/// announce them (story 23-2, threat T1.9 / L8: without it, a Run Key could
/// fan out 1,000 children per level, seven levels deep). The Operator and
/// Host are not capped.
pub const MAX_ROOT_OPEN_DESCENDANTS: u64 = 1000;

/// Who acts on a Run (story 22-1, spine AD-20): the Operator, a Host, or a
/// Run's own Caller presenting its Run Key. A Caller may open, close and
/// tighten only within its own Run's subtree; every Caller-authorized job
/// re-validates the presenting token inside its transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunActor {
    /// The Operator (the CLI, the Control API's operator bearer).
    Operator,
    /// A Host acting on its own Runs.
    Host,
    /// A Run's Caller, authenticated by the Run Key with this hash.
    Caller {
        /// The Run the presented key belongs to.
        run_id: RunId,
        /// The presented key's hash (re-validated in the job).
        token_hash: crate::domain::credential::TokenHash,
    },
}

impl RunActor {
    /// The limits actor this actor writes as (21-6's [`LimitActor`]).
    pub fn limit_actor(&self) -> LimitActor {
        match self {
            RunActor::Operator => LimitActor::Operator,
            RunActor::Host => LimitActor::Host,
            RunActor::Caller { .. } => LimitActor::Caller,
        }
    }

    /// Who issued a credential this actor asked for (`operator`, `host`, or
    /// the presenting key's token id).
    pub(crate) fn issuer(&self) -> String {
        match self {
            RunActor::Operator => "operator".to_string(),
            RunActor::Host => "host".to_string(),
            RunActor::Caller { token_hash, .. } => token_hash.token_id(),
        }
    }
}

/// The identity binding a Host-supplied external id resolves under
/// (`run_identities`, AD-25). Later stories add identity-header and announce
/// bindings beside it.
pub const HOST_IDENTITY_BINDING: &str = "host";

/// How far Hekma controls a Run — a strict ladder, each rung including the
/// one below (PRD Glossary). `supervised (no wire)` is depth `supervised`
/// with `wire_path: false` on every surface (AD-25). `Default` is the
/// ladder's lowest rung (a resolution input with no depth named resolves
/// the least control).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ControlDepth {
    /// Known from its telemetry or announcement only.
    #[default]
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
/// Run by itself. Story 21-2 stores the first three; 21-3 adds the
/// unpriced-model kind; later stories add the unscented, bypass and silent
/// kinds.
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
    /// A resolved Cost Cap has no resolvable Rate (a Policy-supplied cap on
    /// an unpriced model), so the cap cannot fire; cleared when a Rate
    /// resolves (story 21-3 — never silently inert).
    UnpricedModel,
    /// A used refresh credential was presented again (story 22-1): the Run's
    /// credentials were revoked. Cleared by an Operator `revoke-keys`.
    CredentialReuse,
    /// A call was attributed to this Run without a credential Hekma issued
    /// while the Run resolved the `shadow` Scent-Check Mode (story 22-3).
    /// Set by the first such call; cleared when a re-resolution moves the
    /// Run off `shadow`. It never acts — `enforce` refuses, this records.
    UnscentedCall,
}

impl Anomaly {
    const ALL: [Anomaly; 6] = [
        Anomaly::Breach,
        Anomaly::Failed,
        Anomaly::DepthFallback,
        Anomaly::UnpricedModel,
        Anomaly::CredentialReuse,
        Anomaly::UnscentedCall,
    ];

    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Anomaly::Breach => "breach",
            Anomaly::Failed => "failed",
            Anomaly::DepthFallback => "depth-fallback",
            Anomaly::UnpricedModel => "unpriced-model",
            Anomaly::CredentialReuse => "credential-reuse",
            Anomaly::UnscentedCall => "unscented",
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
    /// An Operator endpoint binding's labels (story 23-1): a binding's Run
    /// and its split Runs carry them.
    Binding,
}

impl LabelOrigin {
    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            LabelOrigin::Config => "config",
            LabelOrigin::LabelVerb => "label-verb",
            LabelOrigin::Binding => "binding",
        }
    }

    /// Parse the wire form.
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw {
            "config" => Some(LabelOrigin::Config),
            "label-verb" => Some(LabelOrigin::LabelVerb),
            "binding" => Some(LabelOrigin::Binding),
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

/// The limits set directly on a Run (`run_limits`): on a child Run at open
/// (story 21-2), or through `hekma budget` on any non-ended Run (story
/// 21-6). A limit set here BELONGS TO THE RUN — it ends with the Run, and a
/// fresh start mints a new Run without it. It outranks the instance
/// configuration and every Policy for that limit (FR-60).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLimits {
    /// A per-run token ceiling over the Run's subtree usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_per_run: Option<u64>,
    /// A per-run dollar cap over the Run's subtree cost, in micro-dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dollar_per_run_micros: Option<Micros>,
    /// A cumulative token ceiling, enforced against the hosting instance's
    /// lifetime ledger while this Run is open (story 21-6; process-owning
    /// Runs only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_cumulative: Option<u64>,
    /// A cumulative dollar cap in micro-dollars, enforced against the
    /// hosting instance's lifetime ledger while this Run is open (story
    /// 21-6; process-owning Runs only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dollar_cumulative_micros: Option<Micros>,
    /// The Breach Action for these ceilings (`warn` when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breach_action: Option<BreachAction>,
    /// A Scent-Check Mode set on the Run (story 22-3): only a Caller sets
    /// one, through the limits route, and only to TIGHTEN the Run's
    /// resolved mode — the resolver applies it only where it is stricter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scent_check: Option<policy::ScentCheckMode>,
}

impl RunLimits {
    /// Whether any per-run ceiling is set (a child level's arms: a child
    /// carries no cumulative scope).
    pub fn is_armed(&self) -> bool {
        self.token_per_run.is_some() || self.dollar_per_run_micros.is_some()
    }

    /// Whether any field at all is set (a ceiling of either scope, or the
    /// Breach Action).
    pub fn is_set(&self) -> bool {
        self.token_per_run.is_some()
            || self.dollar_per_run_micros.is_some()
            || self.token_cumulative.is_some()
            || self.dollar_cumulative_micros.is_some()
            || self.breach_action.is_some()
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
    /// Its stored Policy resolution, when one exists (story 21-3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<PolicyResolution>,
    /// Its subtree usage.
    pub usage: RunUsage,
    /// How many of its children have not ended.
    pub open_children: u64,
    /// Story 22-2: its custody, when it owns a supervised process (the
    /// listing's same slice: `on`, or `off` with the reason). Absent on a
    /// Run that owns no process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custody: Option<listing::CustodyView>,
    /// Story 23-2: when its Lease expires (RFC 3339), for a Run that
    /// announced or was heartbeated. Absent on a Run that holds none (a
    /// wire Run goes `silent` on inactivity instead; a supervised Run's
    /// process is its heartbeat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<String>,
    /// Story 23-2: its last identified call or heartbeat (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<String>,
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
    /// Whether to issue the child's Run Key and refresh credential (story
    /// 22-1): returned once by [`crate::Engine::open_child_run_as`].
    /// [`crate::Engine::open_child_run`] never issues them.
    pub issue_credentials: bool,
}

impl ChildRunRequest {
    /// A bare request under `parent`.
    pub fn under(parent: RunParent) -> Self {
        Self {
            parent,
            labels: Vec::new(),
            limits: RunLimits::default(),
            external_id: None,
            issue_credentials: false,
        }
    }
}

/// What a child open returns (story 22-1): the Run, and — when the request
/// asked — its credentials, shown once.
#[derive(Debug)]
pub struct OpenedRun {
    /// The opened (or, on a retried external id, the existing) Run.
    pub view: RunView,
    /// Its Run Key and refresh credential, when issued.
    pub credentials: Option<crate::domain::credential::IssuedCredentials>,
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
    /// Its own limits changed (story 21-6: `hekma budget set`).
    LimitsChanged,
    /// It ended.
    Ended,
    /// An Operator or Host set or cleared one of its wire disposition
    /// reasons (story 23-4): `hold`, `reject` or `reroute`; the cause names
    /// which.
    DispositionChanged,
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
            RunEventKind::LimitsChanged => "limits-changed",
            RunEventKind::Ended => "ended",
            RunEventKind::DispositionChanged => "disposition-changed",
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
    /// Its own limits after the change, on `limits-changed` (story 21-6;
    /// boxed to keep the event small on the bus — the wire form is the
    /// plain object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Box<RunLimits>>,
    /// Its own limits before the change, on `limits-changed` (story 21-6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_limits: Option<Box<RunLimits>>,
    /// Which layer `limits` and `prior_limits` describe, on
    /// `limits-changed` (story 22-1, the 21-6 review's N3): `run` (the
    /// Operator/Host layer) or `caller` (the Caller's own layer, applied
    /// only where tighter). Neither is the effective limit — the Run's
    /// resolution (`hekma budget show`) is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits_layer: Option<String>,
    /// Why (a transition cause, `parent-paused`, `closed`, `label-verb`,
    /// `config`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// The names of the Policies that supplied the Run's limits after the
    /// change (story 21-3, AD-14: `default` counts; empty when none did).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<String>,
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
    /// The Run has ended, so it cannot change (story 21-9: its labels;
    /// story 21-6: its limits and lifecycle verbs too).
    #[error("Run `{run_id}` has ended, so it cannot change")]
    RunEnded {
        /// The Run.
        run_id: String,
    },
    /// A limit scope the Run cannot carry (story 21-6): a cumulative
    /// ceiling belongs to the hosting agent's lifetime ledger, so only its
    /// process-owning Run carries one.
    #[error(
        "`{limit}` cannot be set on Run `{run_id}`: it is a child Run, and a cumulative \
         ceiling counts the hosting agent's lifetime usage — set it on the agent's own Run, \
         or in the agent's `budget.*` configuration"
    )]
    ScopeUnavailable {
        /// The Run.
        run_id: String,
        /// The refused limit (`tokens-cumulative`, …).
        limit: String,
    },
    /// A Caller may only tighten a Run's limits (story 21-6, PRD FR-60).
    #[error(
        "Run `{run_id}`'s limits can only be tightened by its Caller, and this change loosens \
         `{limit}` — an Operator or Host raises a ceiling"
    )]
    RaiseForbidden {
        /// The Run.
        run_id: String,
        /// The first limit the change loosens.
        limit: String,
    },
    /// A lifecycle verb the Run's Control Depth does not serve yet (story
    /// 21-6: the verb × depth table's cell, answered per Run).
    #[error("`{verb}` on Run `{run_id}` is refused: {reason}")]
    VerbUnavailable {
        /// The Run.
        run_id: String,
        /// The verb (`stop`, `pause`, `resume`, `drain`).
        verb: String,
        /// The Run's depth, rendered (`announced`, `supervised (no wire)`).
        depth: String,
        /// Why, naming the depth (or the Metering Source) and the way out.
        reason: String,
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
    /// No Policy has this name (story 21-3).
    #[error("no Policy is named `{name}`")]
    PolicyNotFound {
        /// The name asked for.
        name: String,
    },
    /// A Policy with this name already exists (story 21-3): change it
    /// instead of creating it again.
    #[error(
        "a Policy named `{name}` already exists — change it with `hekma policy change {name}`"
    )]
    PolicyExists {
        /// The existing Policy's name.
        name: String,
    },
    /// The `default` Policy's reserved shape forbids the change (story
    /// 21-3): it cannot be deleted or carry a Selector — it is the
    /// fallback every Run resolves through.
    #[error("the default Policy {what} — it is the fallback every Run resolves through, and every limit it carries applies to every Run")]
    PolicyDefault {
        /// What it cannot do (`"cannot be deleted"`, `"cannot carry a Selector"`).
        what: &'static str,
    },
    /// A listing cursor that is not the codec's form (story 21-5): a bad
    /// base64url body, a foreign version prefix, an empty run id. The typed
    /// `invalid-cursor` refusal — a future key shape never mispages, it
    /// refuses.
    #[error("{detail}")]
    InvalidCursor {
        /// Why the cursor cannot be used.
        detail: String,
    },
    /// A malformed request field.
    #[error("{field}: {reason}")]
    Invalid {
        /// The field (`label`, `token-budget`, …).
        field: String,
        /// Why.
        reason: String,
    },
    /// A Caller acted outside its own Run's subtree (story 22-1).
    #[error(
        "Run `{run_id}` is outside the presenting Run Key's own Run and its children, so the \
         key cannot act on it"
    )]
    RunOutOfScope {
        /// The Run asked for.
        run_id: String,
    },
    /// The presented credential is no longer honoured (story 22-1).
    #[error("the presented credential has been revoked")]
    CredentialRevoked,
    /// The presented credential is not one Hekma issued (story 22-1).
    #[error("the presented credential is not recognised")]
    CredentialUnknown,
    /// A credential could not be issued (story 22-1): the operating system's
    /// random source failed. There is no fallback.
    #[error(
        "the operating system's random source failed, so no credential was issued and nothing \
         was changed ({detail})"
    )]
    CredentialUnavailable {
        /// The source's own error text.
        detail: String,
    },
    /// A Caller opened more open children under one parent than
    /// [`MAX_CALLER_OPEN_CHILDREN`] (story 22-1).
    #[error(
        "Run `{parent}` already has {max} open children opened with a Run Key — close some \
         before opening more"
    )]
    TooManyChildren {
        /// The parent.
        parent: String,
        /// [`MAX_CALLER_OPEN_CHILDREN`].
        max: u64,
    },
    /// A Caller's open or announce found its scope full (story 23-2): a
    /// binding's announced identities, or a root's whole subtree.
    #[error(
        "{scope} already holds {max} open Runs that Callers opened or announced — close some \
         before adding more"
    )]
    TooManyCallerRuns {
        /// The full scope (``binding `openai` `` or ``the Run tree under `run_…` ``).
        scope: String,
        /// The cap.
        max: u64,
    },
    /// The store refused or failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<crate::domain::credential::CredentialError> for RunError {
    fn from(err: crate::domain::credential::CredentialError) -> Self {
        match err {
            crate::domain::credential::CredentialError::RandomSourceFailed { detail } => {
                RunError::CredentialUnavailable { detail }
            }
        }
    }
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
            RunError::ScopeUnavailable { .. } => "scope-unavailable",
            RunError::RaiseForbidden { .. } => "raise-forbidden",
            RunError::VerbUnavailable { .. } => "verb-unavailable",
            RunError::LabelFromConfig { .. } => "label-from-config",
            RunError::LabelNotBound { .. } => "label-not-bound",
            RunError::LabelBound { .. } => "label-bound",
            RunError::TooManyRuns { .. } => "too-many-runs",
            RunError::PolicyNotFound { .. } => "policy-not-found",
            RunError::PolicyExists { .. } => "policy-exists",
            RunError::PolicyDefault { .. } => "policy-default",
            RunError::InvalidCursor { .. } => "invalid-cursor",
            RunError::Invalid { .. } => "invalid-request",
            RunError::RunOutOfScope { .. } => "run-out-of-scope",
            RunError::CredentialRevoked => "credential-revoked",
            RunError::CredentialUnknown => "unauthorized",
            RunError::CredentialUnavailable { .. } => "credential-unavailable",
            RunError::TooManyChildren { .. } | RunError::TooManyCallerRuns { .. } => {
                "too-many-children"
            }
            RunError::Store(_) => "store",
        }
    }
}

/// Why a lifecycle verb addressed to a Run failed (story 21-6): the
/// Run-level refusal (not found, ended, the depth's cell), or the instance
/// verb's own error once the Run mapped to its instance.
#[derive(Debug, thiserror::Error)]
pub enum RunVerbError {
    /// The Run refused the verb.
    #[error(transparent)]
    Run(#[from] RunError),
    /// The instance verb failed.
    #[error(transparent)]
    Engine(#[from] crate::domain::EngineError),
}

#[cfg(test)]
mod keys_tests;
#[cfg(test)]
mod tests;
