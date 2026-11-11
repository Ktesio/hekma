//! The Run listing read (story 21-5, spine AD-25/AD-27, PRD FR-50): ONE
//! exception-first listing behind `hekma ps`, `GET /v0/runs` and the
//! engine's [`list_runs`](crate::Engine::list_runs) facade.
//!
//! The default view is **exception-first**: a Run is listed iff it carries
//! an Anomaly (an ended Run only inside the ended-Run retention window) or
//! holds a `held`/`silent`/`stale` status; healthy open Runs are COUNTED,
//! not listed. `All` lists every Run inside the window. Pages are keyset on
//! `run_id` DESC (newest first — ULID ids encode creation time, and the
//! primary key is the index), with an opaque versioned cursor.
//!
//! [`ENDED_RUN_RETENTION_DAYS`] is the READ-side bound on ended Runs: how
//! long a listing still shows them. Deleting their detail rows (compaction)
//! and the knob for the window belong to the scale story; a knob with no
//! enforcement behind it would hide rows without removing them.
//!
//! Headroom is computed against each Run's STORED resolution
//! ([`PolicyResolution`], never instance configuration alone), so a Policy
//! change re-resolves and the next listing shows it. The budget block
//! reuses [`BudgetView`]'s stable field names plus the per-limit sources,
//! so instance and Run consumers parse one shape.

use serde::{Deserialize, Serialize};

use super::policy::PolicyResolution;
use super::{Label, RunRecord, RunUsage};
use crate::domain::budget::{BreachAction, BreachScope};
use crate::domain::cost::EstimateLabel;
use crate::domain::event::BreachDimension;
use crate::domain::fleet::BudgetView;
use crate::domain::runs::policy::LimitSources;
use crate::ports::PAGE_MAX;

#[cfg(test)]
use super::policy::RunPolicy;
#[cfg(test)]
use crate::domain::cost::Micros;
#[cfg(test)]
use crate::domain::usage::RunId;

/// How long a listing still shows an ended Run (read-side; PRD FR-49's
/// assumed default until compaction lands with its knob).
pub const ENDED_RUN_RETENTION_DAYS: u64 = 7;

/// The page size `hekma ps` and `GET /v0/runs` serve when none is given.
pub const LISTING_DEFAULT_LIMIT: usize = 50;

/// The largest page a listing serves whatever the caller asks for (`0`
/// takes this too). Equals the ports' [`PAGE_MAX`](crate::ports::PAGE_MAX)
/// — the house page cap — by a pinned test.
pub const LISTING_MAX_LIMIT: usize = PAGE_MAX;

/// The cursor codec's version prefix: `base64url("v1:" + run id)`. Any other
/// form (including a future `v2:` shape) is an `invalid-cursor` refusal —
/// versioned so a future key shape never mispages.
const CURSOR_VERSION: &str = "v1:";

/// Which Runs a listing lists (the `view` on the API; `--all` on the CLI).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ListingView {
    /// Anomalies (inside the ended-Run window) and `held`/`silent`/`stale`
    /// statuses; healthy open Runs are counted, not listed.
    #[default]
    Exceptions,
    /// Every Run inside the ended-Run window.
    All,
}

impl ListingView {
    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            ListingView::Exceptions => "exceptions",
            ListingView::All => "all",
        }
    }

    /// Parse the wire form.
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw {
            "exceptions" => Some(ListingView::Exceptions),
            "all" => Some(ListingView::All),
            _ => None,
        }
    }
}

/// One listing request: the view, an optional Selector (21-9 grammar, bound
/// and caller labels), the opaque cursor of the previous page's last row,
/// and the page size (`0` = the maximum).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListRunsQuery {
    /// Which view to list.
    pub view: ListingView,
    /// Only Runs carrying every `key=value` term (bound or caller).
    pub selector: Option<super::Selector>,
    /// The cursor the previous page printed (`None` = the first page).
    pub after: Option<String>,
    /// The page size; `0` takes [`LISTING_MAX_LIMIT`], anything above it
    /// clamps to it.
    pub limit: usize,
}

impl ListRunsQuery {
    /// The default exceptions view: first page, no Selector, default limit.
    pub fn exceptions() -> Self {
        Self {
            view: ListingView::Exceptions,
            selector: None,
            after: None,
            limit: LISTING_DEFAULT_LIMIT,
        }
    }

    /// The effective page size (`0` = the maximum; above it clamps).
    pub fn clamped_limit(&self) -> usize {
        if self.limit == 0 {
            LISTING_MAX_LIMIT
        } else {
            self.limit.min(LISTING_MAX_LIMIT)
        }
    }

    /// The keyset bound this page resumes after (the decoded cursor), or
    /// `None` for the first page. A cursor that is not the codec's form
    /// (a bad base64url body, a foreign version prefix, an empty id) is a
    /// typed refusal — never a silently ignored key.
    pub fn after_key(&self) -> Result<Option<String>, super::RunError> {
        self.after
            .as_deref()
            .map(decode_cursor)
            .transpose()
            .map(|option| option.map(|(run_id, _raw)| run_id))
    }
}

/// The counts a listing reports beside its rows (FR-50's exception-first
/// footer): how many Runs the ACTIVE view lists, and how many are healthy
/// and open (never listed in the exceptions view). Both are subject to the
/// query's Selector.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingCounts {
    /// Runs the active view matches (the whole set, not this page).
    pub listed: u64,
    /// `active` Runs with no Anomaly.
    pub healthy_open: u64,
}

/// One latched breach as a listing row carries it (the fence visibility
/// slice): the `(dimension, scope)` pair that fired and the action it
/// latched under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatchedBreach {
    /// The dimension that fired (`tokens` / `dollars`).
    pub dimension: BreachDimension,
    /// The scope that fired (`per-run` / `cumulative`).
    pub scope: BreachScope,
    /// The action latched when it fired.
    pub action: BreachAction,
}

/// One listing row: the Run's record, its labels, its subtree usage, the
/// latches that hold on it, its headroom against the RESOLVED limits, and —
/// on process-owning rows — the instance's cumulative usage. Cumulative
/// figures appear ONLY on process-owning rows (a shared instance pot must
/// not read as a per-Run figure); on child rows they are absent, never
/// zero.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    /// The Run's stored row (flattened: `run_id`, `status`, … ride the
    /// item directly).
    #[serde(flatten)]
    pub record: RunRecord,
    /// Its labels, bound and caller, sorted by key.
    pub labels: Vec<Label>,
    /// Its usage over its subtree (its own usage plus every descendant's).
    pub usage: RunUsage,
    /// The instance's cumulative usage — present only on process-owning
    /// rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cumulative_usage: Option<RunUsage>,
    /// The breach latches that hold on the Run (`breach[block]` in the
    /// human table).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub latched: Vec<LatchedBreach>,
    /// Headroom against the resolved limits, with each limit's source and
    /// the effective (post depth-fallback) action. `None` when the Run has
    /// no stored resolution; an unpriced Cost Cap shows the
    /// `unpriced-model` Anomaly and no dollar figure, never a fabricated
    /// `$0.00`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<RunBudgetView>,
    /// The Run's instance is kept on `pause` by the breach-default upgrade
    /// (the "kept on pause by upgrade" marker rides the item, not the
    /// compact table).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub kept_on_pause: bool,
}

/// The headroom block a listing row carries: [`BudgetView`]'s stable field
/// names (`per_run_limit`, `per_run_remaining`, `cumulative_limit`, …) plus
/// the per-limit [`LimitSources`] and the effective Breach Action after the
/// depth fallback — one shape for instance and Run consumers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunBudgetView {
    /// The ceilings and remaining values (token and dollar scopes).
    #[serde(flatten)]
    pub view: BudgetView,
    /// Where each resolved limit came from.
    pub sources: LimitSources,
    /// The effective Breach Action after the depth fallback (beside the
    /// resolution's own `breach_action` inside the view).
    pub effective_action: BreachAction,
}

/// One listing page: the rows, the cursor the next page starts after
/// (`None` exactly when the read is exhausted), and the counts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunListing {
    /// The page's rows, newest run id first.
    pub items: Vec<RunSummary>,
    /// The opaque cursor the next page passes as `after` — `None` when the
    /// read is exhausted.
    pub next_cursor: Option<String>,
    /// The view's counts (subject to the query's Selector).
    pub counts: ListingCounts,
}

/// The `--json` / `GET /v0/runs` document: one shape on both surfaces so
/// their items are byte-identical.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunListingDocument {
    /// The listing document family's schema version.
    pub schema_version: u32,
    /// The active view (`exceptions` / `all`).
    pub view: ListingView,
    /// The active Selector, when one filters the listing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<super::Selector>,
    /// The page's rows.
    pub items: Vec<RunSummary>,
    /// The cursor the next page passes (`null` when exhausted).
    pub next_cursor: Option<String>,
    /// The counts.
    pub counts: ListingCounts,
}

/// The listing document family's schema version (AD-14).
pub const RUN_LISTING_SCHEMA_VERSION: u32 = 1;

impl RunListing {
    /// Wrap the listing in its versioned document.
    pub fn into_document(
        self,
        view: ListingView,
        selector: Option<super::Selector>,
    ) -> RunListingDocument {
        RunListingDocument {
            schema_version: RUN_LISTING_SCHEMA_VERSION,
            view,
            selector,
            items: self.items,
            next_cursor: self.next_cursor,
            counts: self.counts,
        }
    }
}

// ---------------------------------------------------------------------------
// The view predicate, the healthy count and the retention window.
// ---------------------------------------------------------------------------

/// Whether a row is listed in the EXCEPTIONS view: an Anomaly (an ended Run
/// only inside the window) or a `held`/`silent`/`stale` status. `silent`
/// and `stale` are STORED statuses — the predicate matches them the moment
/// a producer exists and never computes either at read (AD-25).
///
/// The READ side is the SQL in `store/runs_sqlite.rs` (the v16 partial
/// indexes' arms); this pure predicate is the SPECIFICATION the store
/// tests cross-check the SQL against, row shape by row shape.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn is_exception(
    status: &str,
    anomalies: &str,
    ended_at: Option<&str>,
    cutoff: &str,
) -> bool {
    let loud_status = matches!(status, "held" | "silent" | "stale");
    let anomalous = !anomalies.is_empty()
        && (status != "ended" || ended_at.is_some_and(|ended| ended >= cutoff));
    loud_status || anomalous
}

/// Whether a row is listed in the ALL view: not ended, or ended inside the
/// window. The specification oracle for the SQL arm — see
/// [`is_exception`].
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn in_all_view(status: &str, ended_at: Option<&str>, cutoff: &str) -> bool {
    status != "ended" || ended_at.is_some_and(|ended| ended >= cutoff)
}

/// Whether a row counts as a HEALTHY open Run (counted, never listed in the
/// exceptions view): `active` with no Anomaly. The specification oracle for
/// the count SQL.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn is_healthy_open(status: &str, anomalies: &str) -> bool {
    status == "active" && anomalies.is_empty()
}

/// The ended-Run window's cutoff: `ENDED_RUN_RETENTION_DAYS` before
/// `now_unix_secs`, in the RFC 3339 shape the rows store (whole seconds,
/// `Z`), so SQLite's TEXT comparison is the instant comparison.
pub fn ended_run_cutoff(now_unix_secs: u64) -> String {
    let cutoff = now_unix_secs.saturating_sub(ENDED_RUN_RETENTION_DAYS * 86_400);
    crate::time::format_rfc3339(cutoff)
}

// ---------------------------------------------------------------------------
// The cursor codec: base64url("v1:" + run id), versioned and opaque.
// ---------------------------------------------------------------------------

/// Encode a page's last run id as the cursor the next page passes.
pub fn encode_cursor(run_id: &str) -> String {
    base64url_encode(format!("{CURSOR_VERSION}{run_id}").as_bytes())
}

/// Decode a cursor into `(run id, raw)`. Any other form — a body outside
/// the URL-safe alphabet, a foreign version prefix (`v2:`, or no prefix at
/// all), an empty id — is the typed `invalid-cursor` refusal, so a future
/// key shape never mispages.
pub fn decode_cursor(raw: &str) -> Result<(String, String), super::RunError> {
    let invalid = |reason: String| super::RunError::InvalidCursor {
        detail: format!("the cursor is not one this listing minted ({reason})"),
    };
    let bytes = base64url_decode(raw).map_err(invalid)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| invalid("the decoded cursor is not valid UTF-8".to_string()))?;
    let Some(run_id) = text.strip_prefix(CURSOR_VERSION) else {
        return Err(invalid(
            "an unknown cursor version — re-run the first page".to_string(),
        ));
    };
    if run_id.is_empty() {
        return Err(invalid("the decoded cursor names no run id".to_string()));
    }
    Ok((run_id.to_string(), raw.to_string()))
}

/// The URL-safe base64 alphabet (RFC 4648 §5, unpadded).
const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Encode `bytes` as unpadded base64url. Total, allocation-bounded by the
/// input (a run id: at most a few dozen bytes).
fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let word = (b0 << 16) | (b1 << 8) | b2;
        let quad = [
            B64URL_ALPHABET[(word >> 18) as usize & 0x3f],
            B64URL_ALPHABET[(word >> 12) as usize & 0x3f],
            B64URL_ALPHABET[(word >> 6) as usize & 0x3f],
            B64URL_ALPHABET[word as usize & 0x3f],
        ];
        match chunk.len() {
            1 => out.push_str(std::str::from_utf8(&quad[..2]).expect("alphabet is ASCII")),
            2 => out.push_str(std::str::from_utf8(&quad[..3]).expect("alphabet is ASCII")),
            _ => out.push_str(std::str::from_utf8(&quad).expect("alphabet is ASCII")),
        }
    }
    out
}

/// Decode an unpadded base64url string. Padding (`=`) and every character
/// outside the URL-safe alphabet are errors — the cursor is opaque, and a
/// near-miss must refuse, not guess.
fn base64url_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4 + 3);
    let mut carry: u32 = 0;
    let mut bits: u32 = 0;
    for ch in text.bytes() {
        let value = B64URL_ALPHABET
            .iter()
            .position(|&candidate| candidate == ch)
            .ok_or_else(|| "a character outside the URL-safe base64 alphabet".to_string())?
            as u32;
        carry = (carry << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((carry >> bits) as u8);
        }
    }
    // Reject dangling nonzero bits: a well-formed unpadded body never leaves
    // a nonzero carry behind (a truncated final block).
    if bits > 0 && (carry & ((1 << bits) - 1)) != 0 {
        return Err("a truncated final block".to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Headroom against the RESOLVED limits.
// ---------------------------------------------------------------------------

/// The pure headroom computation for one row (FR-50 / the 21-3 seam):
/// `ceiling − current`, saturating at 0, per scope and dimension, from the
/// Run's STORED resolution — never instance configuration alone. Dollar
/// figures appear only when the resolution carries a Rate; an unpriced cap
/// shows the `unpriced-model` Anomaly and no dollar figure (the resolution
/// itself raises it). Cumulative scopes appear only when the caller passes
/// the instance's cumulative usage — process-owning rows only; a child row
/// omits the cumulative block entirely.
pub(crate) fn headroom(
    resolution: &PolicyResolution,
    subtree: &RunUsage,
    instance_cumulative: Option<&RunUsage>,
) -> Option<RunBudgetView> {
    let subtree_tokens = subtree.input_tokens.saturating_add(subtree.output_tokens);
    let cumulative_tokens =
        instance_cumulative.map(|usage| usage.input_tokens.saturating_add(usage.output_tokens));
    let dollar_active = resolution.rate.is_some() && resolution.dollars.is_set();
    if !resolution.tokens.is_set() && !dollar_active {
        // Nothing is configured: an honest absent budget, never a
        // fabricated ceiling.
        return None;
    }
    let mut view = BudgetView {
        per_run_limit: resolution.tokens.per_run,
        per_run_remaining: resolution
            .tokens
            .per_run
            .map(|ceiling| ceiling.saturating_sub(subtree_tokens)),
        cumulative_limit: None,
        cumulative_remaining: None,
        per_run_cost_cap: None,
        per_run_dollars_remaining: None,
        cumulative_cost_cap: None,
        cumulative_dollars_remaining: None,
        estimate_label: None,
        breach_action: resolution.action,
        policies: Vec::new(),
    };
    if let (Some(ceiling), Some(used)) = (resolution.tokens.cumulative, cumulative_tokens) {
        view.cumulative_limit = Some(ceiling);
        view.cumulative_remaining = Some(ceiling.saturating_sub(used));
    }
    if dollar_active {
        view.per_run_cost_cap = resolution.dollars.per_run;
        view.per_run_dollars_remaining = resolution
            .dollars
            .per_run
            .map(|cap| cap.saturating_remaining(subtree.cost_micros));
        if let Some(usage) = instance_cumulative {
            view.cumulative_cost_cap = resolution.dollars.cumulative;
            view.cumulative_dollars_remaining = resolution
                .dollars
                .cumulative
                .map(|cap| cap.saturating_remaining(usage.cost_micros));
        }
        view.estimate_label = Some(EstimateLabel::Estimated);
    }
    view.policies = resolution.sources.budget_policy_names();
    Some(RunBudgetView {
        view,
        sources: resolution.sources.clone(),
        effective_action: resolution.effective_action,
    })
}

/// Assemble one row's summary from its stored facts (the store layer's
/// per-row point reads in one place): the record, labels, subtree usage,
/// the instance cumulative usage on process-owning rows, the latches, the
/// stored resolution's headroom, and the kept-on-pause marker.
#[allow(clippy::too_many_arguments)]
pub(crate) fn summarize(
    record: RunRecord,
    labels: Vec<Label>,
    subtree: RunUsage,
    instance_cumulative: Option<RunUsage>,
    latches: Vec<LatchedBreach>,
    resolution: Option<&PolicyResolution>,
    kept_on_pause: bool,
) -> RunSummary {
    RunSummary {
        budget: resolution
            .and_then(|resolution| headroom(resolution, &subtree, instance_cumulative.as_ref())),
        record,
        labels,
        usage: subtree,
        cumulative_usage: instance_cumulative,
        latched: latches,
        kept_on_pause,
    }
}

/// The latches as a listing row carries them, sorted by
/// `(dimension, scope)` for a stable render.
pub(crate) fn latched_breaches(
    latches: &[(crate::domain::LatchKey, BreachAction)],
) -> Vec<LatchedBreach> {
    let mut breaches: Vec<LatchedBreach> = latches
        .iter()
        .map(|(key, action)| LatchedBreach {
            dimension: key.dimension,
            scope: key.scope,
            action: *action,
        })
        .collect();
    breaches.sort_by_key(|breach| (breach.dimension.as_str(), breach.scope.as_str()));
    breaches
}

/// Assemble a page's rows from their stored facts (story 21-5): each row's
/// labels, stored resolution, subtree rollup and latches are BOUNDED POINT
/// READS (primary-key probes) over the page (≤ [`LISTING_MAX_LIMIT`] rows),
/// plus — on process-owning rows only — the instance's cumulative rollup.
/// An instance-less Run (a wire Run) reads zero usage until the shared
/// proxy re-keys the rollups: a recorded seam, never a fabricated figure.
pub(crate) fn assemble<S: crate::ports::RunRecords + crate::ports::RunStore + ?Sized>(
    store: &S,
    records: Vec<RunRecord>,
) -> Result<Vec<RunSummary>, crate::ports::StoreError> {
    let usage_of = |rollup: crate::ports::Rollup| RunUsage {
        input_tokens: rollup.totals.input_tokens,
        output_tokens: rollup.totals.output_tokens,
        cached_tokens: rollup.totals.cached_tokens,
        cost_micros: rollup.cost,
        estimate: EstimateLabel::Estimated,
    };
    let mut items = Vec::with_capacity(records.len());
    for record in records {
        let run_id = record.run_id.clone();
        let instance = record.instance.clone();
        let process_owner = record.process_owner;
        let labels = store.run_labels(&run_id)?;
        let resolution = store.run_policy(&run_id)?.map(|row| row.resolution);
        let latches = latched_breaches(&store.latches(&run_id)?);
        let (usage, cumulative_usage) = match instance.as_deref() {
            Some(name) => {
                let iname = crate::domain::InstanceName::new(name).map_err(|err| {
                    crate::ports::StoreError::CorruptRow {
                        name: name.to_string(),
                        detail: format!("invalid instance name on a Run: {err}"),
                    }
                })?;
                let subtree = usage_of(store.run_rollup(&iname, &run_id)?);
                let cumulative = process_owner
                    .then(|| store.instance_rollup(&iname).map(&usage_of))
                    .transpose()?;
                (subtree, cumulative)
            }
            None => (RunUsage::default(), None),
        };
        let kept_on_pause = match instance.as_deref() {
            Some(name) => store.breach_default_kept(name)?,
            None => false,
        };
        items.push(summarize(
            record,
            labels,
            usage,
            cumulative_usage,
            latches,
            resolution.as_ref(),
            kept_on_pause,
        ));
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runs::{Anomaly, RunStatus};

    fn record(status: RunStatus, anomalies: &[Anomaly], ended_at: Option<&str>) -> RunRecord {
        RunRecord {
            run_id: RunId::from_wire("run_01JZ6Q9G3S8V2X7K4M1N5P0R2T"),
            control_depth: crate::domain::ControlDepth::Supervised,
            wire_path: true,
            status,
            anomalies: anomalies.to_vec(),
            parent_run_id: None,
            instance: Some("demo".to_string()),
            process_owner: true,
            end_cause: None,
            created_at: "2026-11-01T00:00:00Z".to_string(),
            updated_at: "2026-11-01T00:00:00Z".to_string(),
            ended_at: ended_at.map(str::to_string),
        }
    }

    #[test]
    fn the_exceptions_predicate_lists_loud_rows_and_recent_anomalies_only() {
        let cutoff = "2026-11-04T00:00:00Z";
        // An active healthy Run: not listed, counted.
        assert!(!is_exception("active", "", None, cutoff));
        assert!(is_healthy_open("active", ""));
        // An active Run with the breach Anomaly: listed.
        assert!(is_exception("active", "breach", None, cutoff));
        // held: listed, Anomaly or not.
        assert!(is_exception("held", "", None, cutoff));
        // silent/stale: listed the moment a producer exists.
        assert!(is_exception("silent", "", None, cutoff));
        assert!(is_exception("stale", "", None, cutoff));
        // ended + failed Anomaly inside the window: listed.
        assert!(is_exception(
            "ended",
            "failed",
            Some("2026-11-09T00:00:00Z"),
            cutoff
        ));
        // ended + Anomaly outside the window: not listed.
        assert!(!is_exception(
            "ended",
            "failed",
            Some("2026-10-20T00:00:00Z"),
            cutoff
        ));
        // ended clean inside the window: not in the exceptions view.
        assert!(!is_exception(
            "ended",
            "",
            Some("2026-11-09T00:00:00Z"),
            cutoff
        ));
        // An ended row with a NULL ended_at (a hand-edited store) is outside
        // the window: the honest read, never a fabricated end time.
        assert!(!is_exception("ended", "failed", None, cutoff));
    }

    #[test]
    fn the_all_view_lists_open_rows_and_recent_ended_rows() {
        let cutoff = "2026-11-04T00:00:00Z";
        assert!(in_all_view("active", None, cutoff));
        assert!(in_all_view("held", None, cutoff));
        assert!(in_all_view("ended", Some("2026-11-09T00:00:00Z"), cutoff));
        assert!(!in_all_view("ended", Some("2026-10-20T00:00:00Z"), cutoff));
        assert!(!in_all_view("ended", None, cutoff));
    }

    #[test]
    fn the_cutoff_is_seven_days_before_the_instant() {
        // 2026-11-11T00:00:00Z minus seven days.
        assert_eq!(ended_run_cutoff(1_794_355_200), "2026-11-04T00:00:00Z");
        // Before the epoch plus seven days: clamps, never underflows.
        assert_eq!(ended_run_cutoff(0), "1970-01-01T00:00:00Z");
        assert_eq!(LISTING_MAX_LIMIT, crate::ports::PAGE_MAX);
    }

    #[test]
    fn the_cursor_codec_round_trips_and_refuses_foreign_shapes() {
        let id = "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T";
        let cursor = encode_cursor(id);
        assert_eq!(decode_cursor(&cursor).unwrap().0, id);
        // Round-trips a legacy id too.
        let legacy = encode_cursor("run-1730000000000000000-7");
        assert_eq!(
            decode_cursor(&legacy).unwrap().0,
            "run-1730000000000000000-7"
        );

        // A future version prefix refuses as invalid-cursor.
        let v2 = base64url_encode(b"v2:run_01JZ6Q9G3S8V2X7K4M1N5P0R2T");
        let err = decode_cursor(&v2).unwrap_err();
        assert_eq!(err.code(), "invalid-cursor");
        // A bare run id (no prefix), padding, a non-URL-safe body, and an
        // empty cursor all refuse with the typed code.
        let padded = format!("{}==", base64url_encode(format!("v1:{id}").as_bytes()));
        let mut standard_alphabet = base64url_encode(b"v1:x");
        standard_alphabet.replace_range(0..1, "+");
        for bad in [
            "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T",
            padded.as_str(),
            standard_alphabet.as_str(),
            "",
            base64url_encode(b"v1:").as_str(),
        ] {
            let err = decode_cursor(bad).unwrap_err();
            assert_eq!(err.code(), "invalid-cursor", "{bad:?}");
        }
        // A truncated final block refuses rather than guessing bytes.
        let dangling = base64url_encode(b"v1:abc").replace("YWJj", "YWJ");
        assert!(decode_cursor(&dangling).is_err(), "{dangling}");
    }

    #[test]
    fn the_base64url_codec_is_pinned_against_the_reference_vectors() {
        // RFC 4648 §10's test vectors, in the URL-safe alphabet (the last
        // uses / -> _).
        assert_eq!(base64url_encode(b""), "");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"fo"), "Zm8");
        assert_eq!(base64url_encode(b"foo"), "Zm9v");
        assert_eq!(base64url_encode(b"foob"), "Zm9vYg");
        assert_eq!(base64url_encode(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64url_encode(b"\xfb\xff"), "-_8");
        for (bytes, text) in [
            (&b""[..], ""),
            (b"f", "Zg"),
            (b"fo", "Zm8"),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg"),
            (b"fooba", "Zm9vYmE"),
            (b"foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64url_decode(text).unwrap(), bytes, "{text}");
        }
        assert!(base64url_decode("Zg==").is_err(), "padding refuses");
        assert!(base64url_decode("Z g").is_err(), "space refuses");
        assert!(base64url_decode("+g").is_err(), "standard alphabet refuses");
    }

    #[test]
    fn the_query_clamps_its_limit_and_decodes_its_cursor() {
        let mut query = ListRunsQuery::exceptions();
        assert_eq!(query.clamped_limit(), LISTING_DEFAULT_LIMIT);
        query.limit = 0;
        assert_eq!(query.clamped_limit(), LISTING_MAX_LIMIT);
        query.limit = LISTING_MAX_LIMIT + 1;
        assert_eq!(query.clamped_limit(), LISTING_MAX_LIMIT);
        query.after = Some(encode_cursor("run_x"));
        assert_eq!(query.after_key().unwrap().as_deref(), Some("run_x"));
        query.after = Some("nonsense".to_string());
        assert_eq!(
            query.after_key().unwrap_err().code(),
            "invalid-cursor",
            "a bad cursor refuses at the query, never mispages"
        );
    }

    #[test]
    fn headroom_computes_against_the_resolved_limits_with_sources() {
        use crate::domain::cost::Rate;
        let resolution = crate::domain::runs::policy::resolve(&{
            let mut input = crate::domain::runs::policy::ResolutionInput {
                depth: crate::domain::ControlDepth::Supervised,
                wire_path: true,
                bound_labels: vec![("team".to_string(), "core".to_string())],
                ..Default::default()
            };
            input.policies = vec![
                crate::domain::runs::PolicyRecord::default_policy(),
                crate::domain::runs::PolicyRecord {
                    name: "tight".to_string(),
                    selector: Some("team=core".parse().unwrap()),
                    limits: crate::domain::runs::PolicyLimits {
                        tokens_per_run: Some(1_000),
                        tokens_cumulative: Some(5_000),
                        dollars_per_run_micros: Some(500_000),
                        rate: Some(Rate::new(1_000_000, 2_000_000)),
                        ..Default::default()
                    },
                    rates: Default::default(),
                },
            ];
            input
        });
        let subtree = RunUsage {
            input_tokens: 300,
            output_tokens: 200,
            cached_tokens: None,
            cost_micros: Micros(100_000),
            estimate: EstimateLabel::Estimated,
        };
        let cumulative = RunUsage {
            input_tokens: 2_000,
            output_tokens: 1_000,
            cached_tokens: None,
            cost_micros: Micros(600_000),
            estimate: EstimateLabel::Estimated,
        };
        let budget = headroom(&resolution, &subtree, Some(&cumulative)).expect("configured");
        assert_eq!(budget.view.per_run_limit, Some(1_000));
        assert_eq!(budget.view.per_run_remaining, Some(500));
        assert_eq!(budget.view.cumulative_limit, Some(5_000));
        assert_eq!(budget.view.cumulative_remaining, Some(2_000));
        assert_eq!(budget.view.per_run_dollars_remaining, Some(Micros(400_000)));
        // The policy sets no cumulative dollar cap: the cumulative dollar
        // fields stay absent even on a process-owning row.
        assert_eq!(budget.view.cumulative_cost_cap, None);
        assert_eq!(budget.view.cumulative_dollars_remaining, None);
        assert!(budget.view.policies.contains(&"tight".to_string()));
        assert!(matches!(
            budget.sources.tokens_per_run,
            crate::domain::runs::LimitSource::Policy(ref name) if name == "tight"
        ));
        assert_eq!(budget.effective_action, crate::domain::BreachAction::Warn);

        // A child row (no cumulative usage) omits the cumulative block
        // entirely — absent, never zero.
        let child = headroom(&resolution, &subtree, None).expect("configured");
        assert_eq!(child.view.cumulative_limit, None);
        assert_eq!(child.view.cumulative_remaining, None);
        assert_eq!(child.view.cumulative_cost_cap, None);
        assert_eq!(child.view.cumulative_dollars_remaining, None);
        assert!(child.view.per_run_limit.is_some());

        // A breached scope saturates at zero remaining, never negative.
        let spent = RunUsage {
            input_tokens: 2_000,
            output_tokens: 0,
            cached_tokens: None,
            cost_micros: Micros(600_000),
            estimate: EstimateLabel::Estimated,
        };
        let breached = headroom(&resolution, &spent, None).expect("configured");
        assert_eq!(breached.view.per_run_remaining, Some(0));
        assert_eq!(breached.view.per_run_dollars_remaining, Some(Micros(0)));

        // An unpriced cap: no dollar figure at all (the resolution raises
        // the unpriced-model Anomaly; the row shows it, not a $0.00).
        let unpriced = crate::domain::runs::policy::resolve(&{
            let mut input = crate::domain::runs::policy::ResolutionInput {
                depth: crate::domain::ControlDepth::Supervised,
                wire_path: true,
                bound_labels: vec![("team".to_string(), "core".to_string())],
                ..Default::default()
            };
            input.policies = vec![
                crate::domain::runs::PolicyRecord::default_policy(),
                crate::domain::runs::PolicyRecord {
                    name: "tight".to_string(),
                    selector: Some("team=core".parse().unwrap()),
                    limits: crate::domain::runs::PolicyLimits {
                        tokens_per_run: Some(1_000),
                        dollars_per_run_micros: Some(500_000),
                        ..Default::default()
                    },
                    rates: Default::default(),
                },
            ];
            input
        });
        assert_eq!(unpriced.raised_anomalies(), vec![Anomaly::UnpricedModel]);
        let budget = headroom(&unpriced, &subtree, Some(&cumulative)).expect("configured");
        assert_eq!(budget.view.per_run_limit, Some(1_000));
        assert_eq!(budget.view.per_run_cost_cap, None);
        assert_eq!(budget.view.per_run_dollars_remaining, None);
        assert_eq!(budget.view.estimate_label, None);

        // Nothing configured: an honest absent budget.
        let bare =
            crate::domain::runs::policy::resolve(&crate::domain::runs::policy::ResolutionInput {
                depth: crate::domain::ControlDepth::Supervised,
                wire_path: true,
                ..Default::default()
            });
        assert!(headroom(&bare, &subtree, Some(&cumulative)).is_none());
    }

    #[test]
    fn summarize_assembles_a_row_with_its_latches_sorted() {
        let resolution =
            crate::domain::runs::policy::resolve(&crate::domain::runs::policy::ResolutionInput {
                depth: crate::domain::ControlDepth::Supervised,
                wire_path: true,
                ..Default::default()
            });
        let latches = vec![
            (
                crate::domain::LatchKey {
                    dimension: crate::domain::BreachDimension::Dollars,
                    scope: crate::domain::BreachScope::PerRun,
                },
                crate::domain::BreachAction::Block,
            ),
            (
                crate::domain::LatchKey {
                    dimension: crate::domain::BreachDimension::Tokens,
                    scope: crate::domain::BreachScope::PerRun,
                },
                crate::domain::BreachAction::Warn,
            ),
        ];
        let summary = summarize(
            record(RunStatus::Active, &[Anomaly::Breach], None),
            vec![],
            RunUsage::default(),
            Some(RunUsage::default()),
            latched_breaches(&latches),
            Some(&resolution),
            true,
        );
        assert_eq!(summary.latched.len(), 2);
        // Sorted by (dimension, scope): dollars sorts before tokens.
        assert_eq!(
            summary.latched[0].dimension,
            crate::domain::BreachDimension::Dollars
        );
        assert_eq!(
            summary.latched[0].action,
            crate::domain::BreachAction::Block
        );
        assert_eq!(
            summary.latched[1].dimension,
            crate::domain::BreachDimension::Tokens
        );
        assert!(summary.kept_on_pause);
        assert!(
            summary.budget.is_none(),
            "an unconfigured resolution has no budget block"
        );
        // The item's JSON flattens the record and carries the latches as
        // objects with all three fields.
        let value = serde_json::to_value(&summary).unwrap();
        assert!(
            value.get("run_id").is_some(),
            "the record is flattened: {value}"
        );
        assert_eq!(value["latched"][0]["dimension"], "dollars");
        assert_eq!(value["latched"][0]["action"], "block");
        assert_eq!(value["latched"][1]["dimension"], "tokens");
        assert_eq!(value["latched"][1]["scope"], "per-run");
        assert_eq!(value["kept_on_pause"], true);
    }

    #[test]
    fn the_document_carries_the_view_the_selector_and_the_counts() {
        let listing = RunListing {
            items: Vec::new(),
            next_cursor: None,
            counts: ListingCounts {
                listed: 3,
                healthy_open: 212,
            },
        };
        let document =
            listing.into_document(ListingView::Exceptions, Some("team=core".parse().unwrap()));
        assert_eq!(document.schema_version, RUN_LISTING_SCHEMA_VERSION);
        let value = serde_json::to_value(&document).unwrap();
        assert_eq!(value["view"], "exceptions");
        assert_eq!(value["selector"], "team=core");
        assert_eq!(value["counts"]["listed"], 3);
        assert_eq!(value["counts"]["healthy_open"], 212);
        assert!(value["next_cursor"].is_null());
        // A listing with no selector omits the field.
        let plain = RunListing {
            items: Vec::new(),
            next_cursor: None,
            counts: ListingCounts::default(),
        }
        .into_document(ListingView::All, None);
        assert!(serde_json::to_value(&plain)
            .unwrap()
            .get("selector")
            .is_none());
    }

    #[test]
    fn the_view_wire_forms_round_trip() {
        assert_eq!(ListingView::Exceptions.as_str(), "exceptions");
        assert_eq!(ListingView::All.as_str(), "all");
        assert_eq!(
            ListingView::from_wire("exceptions"),
            Some(ListingView::Exceptions)
        );
        assert_eq!(ListingView::from_wire("all"), Some(ListingView::All));
        assert!(ListingView::from_wire("everything").is_none());
        assert_eq!(ListingView::default(), ListingView::Exceptions);
    }

    #[test]
    fn a_stored_resolution_round_trips_through_run_policy_serde() {
        // The listing reads resolutions from `run_policy` rows; the stored
        // JSON must survive a round trip so headroom never renders a
        // re-derived guess (a Policy change re-resolves the row itself).
        let resolution =
            crate::domain::runs::policy::resolve(&crate::domain::runs::policy::ResolutionInput {
                depth: crate::domain::ControlDepth::Supervised,
                wire_path: true,
                ..Default::default()
            });
        let row = RunPolicy {
            resolution: resolution.clone(),
            inputs: Default::default(),
            updated_at: "2026-11-11T00:00:00Z".to_string(),
        };
        let text = serde_json::to_string(&row).unwrap();
        let back: RunPolicy = serde_json::from_str(&text).unwrap();
        assert_eq!(back.resolution, resolution);
    }
}
