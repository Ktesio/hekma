//! Inbound telemetry (story 23-3, FR-51, FR-52's third channel, AD-28).
//!
//! A client that only exports OpenTelemetry becomes a Run: the Control
//! API's `/v1/metrics` reads three things from a batch — the
//! `hekma.run.external_id` resource attribute, `hekma.label.*` Caller
//! labels, and the cumulative `gen_ai.client.token_usage` sums by token
//! type — and hands them to [`ingest`], one writer job that:
//!
//! 1. claims or renews the identity through the announce job's telemetry
//!    arm (beneath the credential's binding; credentialed, never minting,
//!    never moving the Run's depth) — a post is a sign of life, so the
//!    Lease renews and a `silent` Run returns to `active`;
//! 2. skips the usage of a Run with a wire path (per-call metering is its
//!    one truth — never summed with telemetry);
//! 3. otherwise diffs each token type's cumulative value against the
//!    durable `telemetry_counters` row and commits the forward difference
//!    as one `telemetry-reported` usage row, always `estimated`, evaluated
//!    over the Run's ancestor chain like a proxied row — in the same
//!    transaction as the counter, which makes a client retry idempotent.
//!
//! A lower cumulative value than stored is a client counter reset: the job
//! re-bases (the new value is the delta) and says so.

use std::sync::Arc;

use super::announce::{announce, AnnounceClaim, AnnounceScope, Claimed};
use super::lease::LeaseOptions;
use super::snapshot::SnapshotPublisher;
use super::{RunError, RunId, RunView};
use crate::domain::credential::AnnounceCredential;
use crate::domain::pipeline::{commit_run_usage, EvaluationInbox, ProxiedUsage, RunUsageRow};
use crate::ports::{RunRecords as _, StoreError};
use crate::store::runs_sqlite::TELEMETRY_SOURCE;
use crate::store::WriterCtx;

/// A token count's type (`gen_ai.token.type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TelemetryTokenType {
    /// Input (prompt) tokens.
    Input,
    /// Output (completion) tokens.
    Output,
    /// The cached subset of the input.
    Cached,
}

impl TelemetryTokenType {
    /// Every type, in the order rows and counters are written.
    pub const ALL: [TelemetryTokenType; 3] = [
        TelemetryTokenType::Input,
        TelemetryTokenType::Output,
        TelemetryTokenType::Cached,
    ];

    /// The wire form (`gen_ai.token.type`, and the counter's key).
    pub fn as_str(&self) -> &'static str {
        match self {
            TelemetryTokenType::Input => "input",
            TelemetryTokenType::Output => "output",
            TelemetryTokenType::Cached => "cached",
        }
    }

    /// Parse the wire form.
    pub fn from_wire(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == raw)
    }
}

/// One cumulative token count from a batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelemetryPoint {
    /// Its token type.
    pub token_type: TelemetryTokenType,
    /// The cumulative value the client's counter reached.
    pub value: u64,
    /// The `gen_ai.request.model` attribute, read for pricing only and
    /// never stored.
    pub model: Option<String>,
}

/// One identity's telemetry from a batch, as the receiver read it.
#[derive(Clone, Debug)]
pub struct TelemetryReport {
    /// The presenting announce credential (its binding fixes the namespace).
    pub credential: AnnounceCredential,
    /// `hekma.run.external_id`.
    pub external_id: String,
    /// `hekma.label.*`, as Caller labels.
    pub labels: Vec<(String, String)>,
    /// The `gen_ai.client.token_usage` points.
    pub points: Vec<TelemetryPoint>,
}

/// What [`ingest`] did.
#[derive(Clone, Debug)]
pub enum TelemetryOutcome {
    /// Positive deltas were committed as one `telemetry-reported` row.
    Committed {
        /// The Run.
        run: Box<RunView>,
        /// The committed deltas.
        input: u64,
        output: u64,
        cached: u64,
        /// A cumulative value went down (the client's counter restarted),
        /// so it was re-based.
        rebased: bool,
        /// Caller labels skipped because the Run's binding sets the key.
        skipped_labels: usize,
    },
    /// Nothing changed since the last post (the Lease still renewed).
    NoDelta {
        /// The Run.
        run: Box<RunView>,
        /// Caller labels skipped because the Run's binding sets the key.
        skipped_labels: usize,
    },
    /// The Run has a wire path: per-call metering is its one truth, so
    /// these points were skipped (the Lease still renewed).
    SkippedWireRun {
        /// The Run.
        run: Box<RunView>,
        /// How many points were skipped.
        points: usize,
    },
}

/// The ingest job's result before the facade reads the Run's view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ingested {
    Committed {
        run: RunId,
        deltas: [u64; 3],
        rebased: bool,
        skipped_labels: usize,
    },
    NoDelta {
        run: RunId,
        skipped_labels: usize,
    },
    SkippedWireRun {
        run: RunId,
        points: usize,
    },
}

/// The forward difference of a cumulative counter: `value - last`, or —
/// when the value went down (the client restarted) — the value itself.
pub(crate) fn delta(last: Option<u64>, value: u64) -> (u64, bool) {
    match last {
        Some(last) if value < last => (value, true),
        Some(last) => (value - last, false),
        None => (value, false),
    }
}

/// The ingest writer job (see the module docs). `Ok(Err(_))` is a refusal
/// with nothing written; `Err(_)` a store failure.
pub(crate) fn ingest(
    ctx: &mut WriterCtx<'_>,
    report: &TelemetryReport,
    run_id: RunId,
    leases: &LeaseOptions,
    now_unix: u64,
    runs: &Arc<SnapshotPublisher>,
    inbox: &Arc<EvaluationInbox>,
) -> Result<Result<Ingested, RunError>, StoreError> {
    let store = ctx.store();
    // The credential is re-read inside the job (time of check).
    match store.announce_credential(&report.credential.hash)? {
        Some((_, _, true)) => {}
        Some(_) => return Ok(Err(RunError::CredentialRevoked)),
        None => return Ok(Err(RunError::CredentialUnknown)),
    }
    let binding = report.credential.binding.clone();
    // A Caller label on a key the binding (or the Run) binds is skipped and
    // counted, never written and never a refusal of the batch.
    let mut bound: Vec<String> = store
        .binding(&binding)?
        .map(|row| row.labels.into_iter().map(|(key, _)| key).collect())
        .unwrap_or_default();
    let namespace = super::bindings::split_namespace(&binding);
    if let Some(existing) = store.identity_run(&namespace, &report.external_id)? {
        bound.extend(
            store
                .run_labels(&existing)?
                .into_iter()
                .filter(|label| label.is_bound())
                .map(|label| label.key),
        );
    }
    let (caller_labels, skipped): (Vec<_>, Vec<_>) = report
        .labels
        .iter()
        .cloned()
        .partition(|(key, _)| !bound.contains(key));
    let claim = AnnounceClaim {
        scope: AnnounceScope::Binding(binding.clone()),
        external: report.external_id.clone(),
        caller_labels,
        credentialed: true,
        created_by: Some(report.credential.hash),
        issuer: report.credential.token_id.clone(),
        wire: false,
        telemetry: true,
    };
    let (claimed, changes) = match announce(store, &claim, run_id, None, leases, now_unix) {
        Ok(done) => done,
        Err(RunError::Store(err)) => return Err(err),
        Err(refusal) => return Ok(Err(refusal)),
    };
    let run = match claimed {
        Claimed::Created { run, .. } | Claimed::Renewed { run } => run,
        Claimed::Capped => {
            return Ok(Err(RunError::TooManyCallerRuns {
                scope: format!("binding `{binding}`"),
                max: super::MAX_CALLER_OPEN_CHILDREN,
            }))
        }
        // The binding is gone, and its credentials with it.
        Claimed::ParentGone => return Ok(Err(RunError::CredentialUnknown)),
    };
    let events = super::ops::commit(ctx, runs, changes)?;
    let events_inbox = Arc::clone(inbox);
    ctx.after_commit(move || events_inbox.push_run_events(events));
    let store = ctx.store();
    let record = store.run_record(&run)?.ok_or_else(|| {
        StoreError::Backend(format!("the Run {} vanished in its job", run.as_str()))
    })?;
    if record.wire_path {
        return Ok(Ok(Ingested::SkippedWireRun {
            run,
            points: report.points.len(),
        }));
    }
    // One cumulative sum per token type over the batch's points.
    let mut sums = [None::<u64>; 3];
    for point in &report.points {
        let slot = &mut sums[point.token_type as usize];
        *slot = Some(slot.unwrap_or(0).saturating_add(point.value));
    }
    let at = crate::time::format_rfc3339(now_unix);
    let mut deltas = [0u64; 3];
    let mut rebased = false;
    for token_type in TelemetryTokenType::ALL {
        let index = token_type as usize;
        let Some(value) = sums[index] else {
            continue;
        };
        let last = store.telemetry_counter(&run, token_type.as_str())?;
        let (d, reset) = delta(last, value);
        deltas[index] = d;
        rebased |= reset;
        store.set_telemetry_counter(&run, token_type.as_str(), value, &at)?;
    }
    let skipped_labels = skipped.len();
    if deltas.iter().all(|d| *d == 0) {
        return Ok(Ok(Ingested::NoDelta {
            run,
            skipped_labels,
        }));
    }
    // One model for the batch prices it; several (or none) price unpriced
    // unless the Run's Policy sets a flat Rate.
    let mut models = report.points.iter().filter_map(|p| p.model.as_deref());
    let first = models.next();
    let model = first
        .filter(|first| models.all(|other| other == *first))
        .map(str::to_string);
    commit_run_usage(
        ctx,
        RunUsageRow {
            run_id: run.clone(),
            usage: ProxiedUsage {
                input: deltas[0],
                output: deltas[1],
                cached: deltas[2],
            },
            source: TELEMETRY_SOURCE,
            call_id: None,
            model,
            occurred_at: at,
        },
        runs,
        inbox,
    )?;
    Ok(Ok(Ingested::Committed {
        run,
        deltas,
        rebased,
        skipped_labels,
    }))
}

#[cfg(test)]
#[path = "telemetry_tests.rs"]
mod tests;
