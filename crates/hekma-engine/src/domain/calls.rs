//! Per-call records of the shared Metering Proxy (story 23-1, spine AD-26):
//! the data a call-start and a call-close record carry to the calls store,
//! and — since the read surface — the query, the page and the document a
//! Host reads them through.
//!
//! A record never holds a request or response byte, a header value or a
//! token: identifiers, the Run's Control Depth, the binding name, the
//! disposition and outcome, the status, the model (capped and
//! charset-checked), token counts and timings. There is no dollar figure:
//! derived dollars stay on the ledger and the Run; the call id is the join
//! key.
//!
//! The reader side (the per-call read surface over the calls store) keeps
//! the same discipline: pages are keyset on the call id (descending — a
//! call id is a ULID minted at admission, so descending id is descending
//! start time to the millisecond), every page is bounded, and a filter that
//! matches sparsely stops after a fixed scan budget and says so.

use serde::{Deserialize, Serialize};

use super::RunId;
use crate::ports::ProcessFingerprint;

/// The longest model name a record keeps (visible ASCII only).
pub const MAX_MODEL_CHARS: usize = 128;

/// The page size the calls listing serves when none is given.
pub const CALLS_DEFAULT_LIMIT: usize = 50;

/// The largest calls page whatever the caller asks for (`0` takes this
/// too). Equals the ports' [`PAGE_MAX`](crate::ports::PAGE_MAX) — the house
/// page cap — by a pinned test.
pub const CALLS_MAX_LIMIT: usize = crate::ports::PAGE_MAX;

/// How many rows one calls page examines at most across segments: a filter
/// that matches sparsely would otherwise walk a whole history. When the
/// budget runs out before the page fills, the page returns what it found
/// with `scan_limited: true` and a cursor that continues. A constant, not a
/// user setting — it protects the Host from a careless query. Tests lower
/// it through the store's `#[cfg(test)]` seam.
pub const CALLS_SCAN_BUDGET: usize = 100_000;

/// The calls listing document family's schema version.
pub const CALLS_LISTING_SCHEMA_VERSION: u32 = 1;

/// What the proxy did with a call before forwarding.
///
/// Forward-compatible: a value a newer build writes renders as its raw
/// string ([`CallDisposition::Other`]), is never an error on read, and is
/// refused as a FILTER value (a filter names a closed list this build
/// knows). `hold`, `reroute` and `reject` are in the shape now — the
/// fence stories that produce them fill them in later.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallDisposition {
    /// Forwarded.
    Pass,
    /// Refused by a `block` reason on the Run or an ancestor.
    Block,
    /// Held for release (produced by a later story; never written yet).
    Hold,
    /// Rerouted to another endpoint (produced by a later story).
    Reroute,
    /// Rejected (produced by a later story).
    Reject,
    /// A value this build does not know (read from a newer build's store).
    #[serde(untagged)]
    Other(String),
}

impl CallDisposition {
    /// The stable wire form (the stored text; `Other` echoes its raw value).
    pub fn as_str(&self) -> &str {
        match self {
            CallDisposition::Pass => "pass",
            CallDisposition::Block => "block",
            CallDisposition::Hold => "hold",
            CallDisposition::Reroute => "reroute",
            CallDisposition::Reject => "reject",
            CallDisposition::Other(raw) => raw,
        }
    }

    /// Parse a FILTER value: only the closed list this build knows — an
    /// unknown value is the typed `invalid-filter` refusal, never a filter
    /// that silently matches nothing.
    pub fn from_filter(raw: &str) -> Option<Self> {
        match raw {
            "pass" => Some(CallDisposition::Pass),
            "block" => Some(CallDisposition::Block),
            "hold" => Some(CallDisposition::Hold),
            "reroute" => Some(CallDisposition::Reroute),
            "reject" => Some(CallDisposition::Reject),
            _ => None,
        }
    }

    /// Read a stored value: anything parses (an unknown value renders as
    /// its raw string and is never an error).
    pub fn from_raw(raw: &str) -> Self {
        Self::from_filter(raw).unwrap_or_else(|| CallDisposition::Other(raw.to_string()))
    }
}

/// How a call ended.
///
/// Forward-compatible the same way [`CallDisposition`] is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallOutcome {
    /// The upstream answered and the response was relayed.
    Completed,
    /// The proxy refused it after identifying its Run.
    Refused,
    /// The upstream could not be reached or failed mid-response.
    UpstreamError,
    /// The client went away while the call was upstream.
    ClientDisconnect,
    /// The host shut down with the call in flight.
    Shutdown,
    /// Closed at the next open: its owner died and the ledger has no usage
    /// row for it (tokens unknown).
    Crash,
    /// Closed at the next open: its owner died after its usage committed.
    Recovered,
    /// A value this build does not know (read from a newer build's store).
    #[serde(untagged)]
    Other(String),
}

impl CallOutcome {
    /// The stable wire form (`Other` echoes its raw value).
    pub fn as_str(&self) -> &str {
        match self {
            CallOutcome::Completed => "completed",
            CallOutcome::Refused => "refused",
            CallOutcome::UpstreamError => "upstream-error",
            CallOutcome::ClientDisconnect => "client-disconnect",
            CallOutcome::Shutdown => "shutdown",
            CallOutcome::Crash => "crash",
            CallOutcome::Recovered => "recovered",
            CallOutcome::Other(raw) => raw,
        }
    }

    /// Parse a FILTER value (the closed list this build knows).
    pub fn from_filter(raw: &str) -> Option<Self> {
        match raw {
            "completed" => Some(CallOutcome::Completed),
            "refused" => Some(CallOutcome::Refused),
            "upstream-error" => Some(CallOutcome::UpstreamError),
            "client-disconnect" => Some(CallOutcome::ClientDisconnect),
            "shutdown" => Some(CallOutcome::Shutdown),
            "crash" => Some(CallOutcome::Crash),
            "recovered" => Some(CallOutcome::Recovered),
            _ => None,
        }
    }

    /// Read a stored value: anything parses.
    pub fn from_raw(raw: &str) -> Self {
        Self::from_filter(raw).unwrap_or_else(|| CallOutcome::Other(raw.to_string()))
    }
}

/// A call's status class.
///
/// Forward-compatible the same way [`CallDisposition`] is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusClass {
    Success,
    Redirect,
    ClientError,
    ServerError,
    /// Refused by the proxy (no upstream status).
    Refused,
    /// No upstream status (connect, TLS or mid-body failure, disconnect).
    TransportError,
    /// A value this build does not know (read from a newer build's store).
    #[serde(untagged)]
    Other(String),
}

impl StatusClass {
    /// The class of an upstream status code.
    pub(crate) fn of_status(status: u16) -> Self {
        match status {
            300..=399 => StatusClass::Redirect,
            400..=499 => StatusClass::ClientError,
            500..=599 => StatusClass::ServerError,
            _ => StatusClass::Success,
        }
    }

    /// The stable wire form (`Other` echoes its raw value).
    pub fn as_str(&self) -> &str {
        match self {
            StatusClass::Success => "2xx",
            StatusClass::Redirect => "3xx",
            StatusClass::ClientError => "4xx",
            StatusClass::ServerError => "5xx",
            StatusClass::Refused => "refused",
            StatusClass::TransportError => "transport-error",
            StatusClass::Other(raw) => raw,
        }
    }

    /// Parse a FILTER value (the closed list this build knows).
    pub fn from_filter(raw: &str) -> Option<Self> {
        match raw {
            "2xx" => Some(StatusClass::Success),
            "3xx" => Some(StatusClass::Redirect),
            "4xx" => Some(StatusClass::ClientError),
            "5xx" => Some(StatusClass::ServerError),
            "refused" => Some(StatusClass::Refused),
            "transport-error" => Some(StatusClass::TransportError),
            _ => None,
        }
    }

    /// Read a stored value: anything parses.
    pub fn from_raw(raw: &str) -> Self {
        Self::from_filter(raw).unwrap_or_else(|| StatusClass::Other(raw.to_string()))
    }
}

/// The `problems` predicate's exact index text: a call is a problem when it
/// was not a clean pass. The partial index and the `problems` statement
/// carry this text LITERALLY — SQLite uses a partial index only when the
/// statement carries its predicate as written, and the plan pin catches a
/// drift. A later story that widens the predicate re-creates the index in
/// the same additive step style.
pub(crate) const CALLS_PROBLEM_WHERE: &str =
    "disposition <> 'pass' OR outcome <> 'completed' OR status_class NOT IN ('2xx','3xx')";

/// Whether a call id has the shape the proxy mints: `call_` + 26 Crockford
/// base32 characters (a ULID). Ids are opaque to the store; this is the
/// shape check the read surfaces and the cursor codec make, so a calls
/// cursor and a Run cursor can never be confused in either direction.
pub fn is_call_id(raw: &str) -> bool {
    raw.strip_prefix("call_").is_some_and(|rest| {
        rest.len() == 26 && rest.bytes().all(|b| crate::domain::CROCKFORD.contains(&b))
    })
}

/// Whether a filter's Run id could name a Run this engine minted: the ULID
/// shape (`run_` + 26 Crockford characters) or the legacy
/// `run-<nanos>-<nonce>` shape. Anything else is the typed `invalid-run`
/// refusal — a filter that could never match is a refused value, never a
/// silently empty page.
pub fn is_run_id(raw: &str) -> bool {
    if raw.strip_prefix("run_").is_some_and(|rest| {
        rest.len() == 26 && rest.bytes().all(|b| crate::domain::CROCKFORD.contains(&b))
    }) {
        return true;
    }
    match raw.strip_prefix("run-") {
        Some(rest) => match rest.split_once('-') {
            Some((nanos, nonce)) => {
                !nanos.is_empty()
                    && !nonce.is_empty()
                    && nanos.bytes().all(|b| b.is_ascii_digit())
                    && nonce.bytes().all(|b| b.is_ascii_digit())
            }
            None => false,
        },
        None => false,
    }
}

/// The calls listing's view: every record, or only the ones that were not a
/// clean pass. `All` is the default — a Host asking for a Run's calls
/// expects all of them; `problems` is the exception-first opt-in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CallView {
    /// Every record the filters match.
    #[default]
    All,
    /// Only records that were not a clean pass: refused, upstream-error,
    /// disconnect, a `4xx`/`5xx` status, `crash` and `recovered`.
    Problems,
}

impl CallView {
    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            CallView::All => "all",
            CallView::Problems => "problems",
        }
    }

    /// Parse the wire form.
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw {
            "all" => Some(CallView::All),
            "problems" => Some(CallView::Problems),
            _ => None,
        }
    }
}

/// One calls listing request: the filters (each optional, all conjunctive),
/// the call-start window (half-open: `from` inclusive, `to` exclusive, RFC
/// 3339), the view, the opaque cursor of the previous page's last row, and
/// the page size (`0` = the maximum). The closed-list filters
/// (`outcome`, `disposition`, `status_class`, `depth`) are validated by the
/// engine — one validation site for the API, the CLI and embedders.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallsQuery {
    /// Only calls attributed to this Run (never a subtree: a parent's
    /// listing returns only its own calls).
    pub run: Option<String>,
    /// Only calls that presented this Run Key (its 12-hex token id label —
    /// an equality on the stored label, never a lookup key).
    pub token_id: Option<String>,
    /// Only calls with this outcome (`completed`, `refused`, …).
    pub outcome: Option<String>,
    /// Only calls with this disposition (`pass`, `block`, …).
    pub disposition: Option<String>,
    /// Only calls with this status class (`2xx`, `429`-shaped `4xx`, …).
    pub status_class: Option<String>,
    /// Only calls whose model text equals this (client-supplied text).
    pub model: Option<String>,
    /// Only calls at this Control Depth (`observed`, `wire`, `announced`,
    /// `supervised`).
    pub depth: Option<String>,
    /// Only calls off this endpoint binding (by name); absent lists calls
    /// with no binding too when no other filter narrows them.
    pub binding: Option<String>,
    /// The call-start window's inclusive lower bound (RFC 3339).
    pub from: Option<String>,
    /// The call-start window's exclusive upper bound (RFC 3339).
    pub to: Option<String>,
    /// Which view to list.
    pub view: CallView,
    /// The cursor the previous page printed (`None` = the first page).
    pub after: Option<String>,
    /// The page size; `0` takes [`CALLS_MAX_LIMIT`], anything above it
    /// clamps to it.
    pub limit: usize,
}

impl CallsQuery {
    /// The first page of every call, at the default limit.
    pub fn all() -> Self {
        Self {
            limit: CALLS_DEFAULT_LIMIT,
            ..CallsQuery::default()
        }
    }

    /// The effective page size (`0` = the maximum; above it clamps).
    pub fn clamped_limit(&self) -> usize {
        if self.limit == 0 {
            CALLS_MAX_LIMIT
        } else {
            self.limit.min(CALLS_MAX_LIMIT)
        }
    }

    /// The filters this query applies, as a listing document echoes them
    /// (one construction site for every surface).
    pub fn echoed_filters(&self) -> CallFilters {
        CallFilters {
            run: self.run.clone(),
            token_id: self.token_id.clone(),
            outcome: self.outcome.clone(),
            disposition: self.disposition.clone(),
            status_class: self.status_class.clone(),
            model: self.model.clone(),
            depth: self.depth.clone(),
            binding: self.binding.clone(),
            from: self.from.clone(),
            to: self.to.clone(),
            view: self.view,
            limit: self.clamped_limit(),
        }
    }
}

/// The calls read's typed refusals (the listing and the by-id probe share
/// them). Every variant carries the stable machine code the Control API
/// serves as `error.code`.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// A cursor that is not the codec's form, or one whose id is not a
    /// call id (a Run cursor): the typed `invalid-cursor` refusal, never a
    /// silently ignored key.
    #[error("{detail}")]
    InvalidCursor {
        /// Why the cursor cannot be used.
        detail: String,
    },
    /// A call id that is not the shape the proxy mints.
    #[error("{call_id} is not a call id this engine records (call_ plus 26 characters)")]
    InvalidCallId {
        /// The refused value.
        call_id: String,
    },
    /// A `run` filter that could not name a Run this engine minted.
    #[error("{value} does not name a Run this engine recorded")]
    InvalidRun {
        /// The refused value.
        value: String,
    },
    /// A `from`/`to` window bound that is not RFC 3339, or a `to` at or
    /// before the `from`.
    #[error("{detail}")]
    InvalidTime {
        /// Why the window cannot be used.
        detail: String,
    },
    /// A closed-list filter value this build does not know (`outcome`,
    /// `disposition`, `status_class`, `depth`).
    #[error("{param} must be one of the values this build knows: {detail}")]
    InvalidFilter {
        /// The parameter's name (`outcome`, `disposition`, …).
        param: &'static str,
        /// The accepted values / why the given one was refused.
        detail: String,
    },
    /// No record has the call id.
    #[error("no call has the id {call_id}")]
    NotFound {
        /// The asked-for call id.
        call_id: String,
    },
    /// The calls store could not be read (a segment failed to open in a way
    /// that is not "skip it"): retryable.
    #[error("the calls store could not be read: {detail}")]
    Unavailable {
        /// The surfaced reason.
        detail: String,
    },
}

impl CallError {
    /// The stable machine code for wire surfaces.
    pub fn code(&self) -> &'static str {
        match self {
            CallError::InvalidCursor { .. } => "invalid-cursor",
            CallError::InvalidCallId { .. } => "invalid-call-id",
            CallError::InvalidRun { .. } => "invalid-run",
            CallError::InvalidTime { .. } => "invalid-time",
            CallError::InvalidFilter { .. } => "invalid-filter",
            CallError::NotFound { .. } => "call-not-found",
            CallError::Unavailable { .. } => "calls-unavailable",
        }
    }
}

/// One calls listing page: the records, the cursor the next page starts
/// after (`None` exactly when the read is exhausted), whether the scan
/// budget stopped the page before it filled, how far back the page looked
/// when it did, and how many segment files were skipped.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallListing {
    /// The page's records, newest call id first.
    pub items: Vec<CallRecord>,
    /// The opaque cursor the next page passes as `cursor` — `None` when
    /// the read is exhausted.
    pub next_cursor: Option<String>,
    /// The scan budget stopped this page before it filled (a sparse
    /// filter): the page is partial, the cursor continues, and the client
    /// may fetch more.
    pub scan_limited: bool,
    /// The start time of the last row examined when `scan_limited` is set
    /// (how far back this page looked).
    pub searched_back_to: Option<String>,
    /// How many segment files were skipped (unreadable, or written by a
    /// newer Hekma): the listing is partial and says so.
    pub skipped_segments: u64,
    /// The skipped files' names and reasons (surfaced by the CLI's note;
    /// not part of the wire document).
    #[serde(skip)]
    pub skipped_files: Vec<String>,
}

impl CallListing {
    /// Wrap the listing in its versioned document, echoing the filters
    /// that were applied so a saved page is self-describing.
    pub fn into_document(self, filters: CallFilters) -> CallListingDocument {
        CallListingDocument {
            schema_version: CALLS_LISTING_SCHEMA_VERSION,
            filters,
            skipped_segments: self.skipped_segments,
            items: self.items,
            next_cursor: self.next_cursor,
            scan_limited: self.scan_limited,
            searched_back_to: self.searched_back_to,
            skipped_files: self.skipped_files,
        }
    }
}

/// The filters a calls listing applied, echoed by the document (absent
/// fields were not applied).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallFilters {
    /// The Run filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// The token-id label filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_id: Option<String>,
    /// The outcome filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The disposition filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    /// The status-class filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_class: Option<String>,
    /// The model filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The Control Depth filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<String>,
    /// The binding filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    /// The window's inclusive lower bound, as given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The window's exclusive upper bound, as given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// The view (`all` / `problems`).
    pub view: CallView,
    /// The page size served.
    pub limit: usize,
}

/// The `--json` / `GET /v0/calls` document: one shape on both surfaces so
/// their items are byte-identical.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallListingDocument {
    /// The listing document family's schema version.
    pub schema_version: u32,
    /// The filters that were applied (echoed).
    pub filters: CallFilters,
    /// How many segment files were skipped.
    pub skipped_segments: u64,
    /// The page's records.
    pub items: Vec<CallRecord>,
    /// The cursor the next page passes (`null` when exhausted).
    pub next_cursor: Option<String>,
    /// Whether the scan budget stopped this page.
    pub scan_limited: bool,
    /// How far back a scan-limited page looked.
    pub searched_back_to: Option<String>,
    /// The skipped files' names (surfaced; never bodies).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skipped_files: Vec<String>,
}

// ---------------------------------------------------------------------------
// The cursor wrappers: the Run listing's codec, with the id required to be
// a call id (so a Run cursor and a calls cursor cannot be confused in
// either direction).
// ---------------------------------------------------------------------------

/// Encode a page's last call id as the cursor the next page passes
/// (`base64url("v1:" + id)` — the Run listing's codec unchanged).
pub fn encode_call_cursor(call_id: &str) -> String {
    crate::encode_cursor(call_id)
}

/// Decode a calls cursor into its call id. Any other form — the codec's own
/// refusals, or an id that is not a call id (a Run cursor) — is the typed
/// `invalid-cursor` refusal.
pub fn decode_call_cursor(raw: &str) -> Result<String, CallError> {
    let (id, _raw) = crate::decode_cursor(raw).map_err(|err| CallError::InvalidCursor {
        detail: err.to_string(),
    })?;
    if !is_call_id(&id) {
        return Err(CallError::InvalidCursor {
            detail: "the cursor names an id of another kind — it is not one this listing \
                     printed (start again from the first page)"
                .to_string(),
        });
    }
    Ok(id)
}

// ---------------------------------------------------------------------------
// The model mask: client-supplied text, masked when it is shaped like a
// Hekma credential.
// ---------------------------------------------------------------------------

/// The value a credential-shaped model text renders as.
pub const MODEL_REDACTED: &str = "[redacted]";

/// Render a stored `model` value for exposure: masked with
/// `[redacted]` when it carries a Hekma token shape (the exact shape
/// [`parse_presented`](crate::parse_presented) knows — a client could paste
/// a key into the model field), and any character outside visible ASCII
/// replaced with `?` (a tampered or foreign file). The value is client text
/// the docs say is shown as recorded; these two shapes are the exceptions.
/// A provider key of another shape (`sk-…`) cannot be recognised and is not
/// masked.
pub fn expose_model(raw: &str) -> String {
    if crate::parse_presented(raw).is_some() {
        return MODEL_REDACTED.to_string();
    }
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_graphic() || ch == ' ' {
                ch
            } else {
                '?'
            }
        })
        .collect()
}

/// What a call-start record carries (an `open_calls` row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallStart {
    pub call_id: String,
    pub run_id: RunId,
    pub token_id: Option<String>,
    pub control_depth: &'static str,
    pub binding: Option<String>,
    pub started_at: String,
    /// The proxy-owning process (closure at the next open reads it).
    pub owner: Option<ProcessFingerprint>,
}

/// What a call-close record carries (a `calls` row) — and what the read
/// surfaces expose. `hold_end` is in the shape now and stays `None` until
/// the hold fence fills it; the token counts are `None` when unknown,
/// never zero (a known zero is `Some(0)`); `input_tokens` includes the
/// cached subset, as everywhere else, so the three never sum.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRecord {
    /// `call_` + ULID, minted at admission.
    pub call_id: String,
    /// The Run the call was attributed to.
    pub run_id: RunId,
    /// The 12-hex label of the Run Key the call presented, `None` when it
    /// presented none (a call attributed by Run route, binding or the
    /// unattributed Run has no key). A display label, never a secret.
    pub token_id: Option<String>,
    /// The Run's Control Depth at the call (`observed` / `wire` /
    /// `announced` / `supervised`; a value a newer build wrote renders as
    /// its raw string).
    pub control_depth: String,
    /// The binding name, `None` off a binding.
    pub binding: Option<String>,
    /// What the proxy did with the call.
    pub disposition: CallDisposition,
    /// When a hold would end — always `None` until the hold fence ships.
    pub hold_end: Option<String>,
    /// How the call ended.
    pub outcome: CallOutcome,
    /// The HTTP status the client got, `None` when none.
    pub status: Option<u16>,
    /// The status class.
    pub status_class: StatusClass,
    /// The model the response or request named (client-supplied text), as
    /// stored.
    pub model: Option<String>,
    /// Input tokens (includes the cached subset); `None` = unknown.
    pub input_tokens: Option<u64>,
    /// Output tokens; `None` = unknown.
    pub output_tokens: Option<u64>,
    /// The cached subset of the input tokens; `None` = unknown.
    pub cached_tokens: Option<u64>,
    /// When the call was admitted (RFC 3339 UTC with fractional seconds,
    /// as stored).
    pub started_at: String,
    /// When the call closed (RFC 3339 UTC with fractional seconds).
    pub ended_at: String,
    /// The whole call as the client saw it, in microseconds.
    pub latency_us: u64,
    /// The upstream leg, in microseconds; `None` when no upstream was
    /// dialled.
    pub upstream_us: Option<u64>,
}

/// A model name fit for a record: visible ASCII, at most
/// [`MAX_MODEL_CHARS`]; anything else is dropped (never stored raw).
pub(crate) fn record_model(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_MODEL_CHARS
        || !trimmed.bytes().all(|b| b.is_ascii_graphic())
    {
        return None;
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classes_and_names_are_the_wire_strings() {
        assert_eq!(StatusClass::of_status(200).as_str(), "2xx");
        assert_eq!(StatusClass::of_status(307).as_str(), "3xx");
        assert_eq!(StatusClass::of_status(429).as_str(), "4xx");
        assert_eq!(StatusClass::of_status(503).as_str(), "5xx");
        assert_eq!(StatusClass::of_status(101).as_str(), "2xx");
        assert_eq!(StatusClass::Refused.as_str(), "refused");
        assert_eq!(StatusClass::TransportError.as_str(), "transport-error");
        for (outcome, name) in [
            (CallOutcome::Completed, "completed"),
            (CallOutcome::Refused, "refused"),
            (CallOutcome::UpstreamError, "upstream-error"),
            (CallOutcome::ClientDisconnect, "client-disconnect"),
            (CallOutcome::Shutdown, "shutdown"),
            (CallOutcome::Crash, "crash"),
            (CallOutcome::Recovered, "recovered"),
        ] {
            assert_eq!(outcome.as_str(), name);
        }
        assert_eq!(CallDisposition::Pass.as_str(), "pass");
        assert_eq!(CallDisposition::Block.as_str(), "block");
    }

    #[test]
    fn a_model_is_kept_only_when_short_visible_ascii() {
        assert_eq!(record_model(" gpt-4o "), Some("gpt-4o".to_string()));
        assert_eq!(record_model(""), None);
        assert_eq!(record_model("a b"), None);
        assert_eq!(record_model("модель"), None);
        assert_eq!(record_model(&"m".repeat(129)), None);
        assert!(record_model(&"m".repeat(128)).is_some());
    }

    #[test]
    fn the_enums_render_unknown_values_and_refuse_them_as_filters() {
        // Read: an unknown value renders as its raw string, never an error.
        assert_eq!(CallOutcome::from_raw("superceded").as_str(), "superceded");
        assert_eq!(
            CallDisposition::from_raw("quarantine").as_str(),
            "quarantine"
        );
        assert_eq!(StatusClass::from_raw("1xx").as_str(), "1xx");
        // Filter: the closed list this build knows; unknown refuses.
        assert!(CallOutcome::from_filter("completed").is_some());
        assert!(CallOutcome::from_filter("superceded").is_none());
        assert!(CallDisposition::from_filter("pass").is_some());
        assert!(CallDisposition::from_filter("hold").is_some());
        assert!(CallDisposition::from_filter("quarantine").is_none());
        assert!(StatusClass::from_filter("2xx").is_some());
        assert!(StatusClass::from_filter("1xx").is_none());
        // Round trip: every filterable value reads back as itself.
        for raw in [
            "completed",
            "refused",
            "upstream-error",
            "client-disconnect",
            "shutdown",
            "crash",
            "recovered",
        ] {
            assert_eq!(CallOutcome::from_filter(raw).unwrap().as_str(), raw);
        }
        for raw in ["pass", "block", "hold", "reroute", "reject"] {
            assert_eq!(CallDisposition::from_filter(raw).unwrap().as_str(), raw);
        }
        for raw in ["2xx", "3xx", "4xx", "5xx", "refused", "transport-error"] {
            assert_eq!(StatusClass::from_filter(raw).unwrap().as_str(), raw);
        }
        // A record's unknown values survive a JSON round trip.
        let record = CallRecord {
            call_id: "call_1".to_string(),
            run_id: RunId::from_wire("run_A"),
            token_id: None,
            control_depth: "wire".to_string(),
            binding: None,
            disposition: CallDisposition::from_raw("quarantine"),
            hold_end: None,
            outcome: CallOutcome::from_raw("superceded"),
            status: None,
            status_class: StatusClass::from_raw("1xx"),
            model: None,
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            started_at: "t".to_string(),
            ended_at: "t".to_string(),
            latency_us: 0,
            upstream_us: None,
        };
        let back: CallRecord =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).expect("round trips");
        assert_eq!(back, record);
    }

    #[test]
    fn call_ids_are_recognised_by_shape_and_run_ids_by_either_shape() {
        let call = format!("call_{}", crate::domain::mint_ulid());
        assert!(is_call_id(&call));
        assert!(!is_call_id("call_01JZ6Q9G3S8V2X7K4M1N5P0R"), "25 chars");
        assert!(!is_call_id("call_01JZ6Q9G3S8V2X7K4M1N5P0R2"), "27 chars");
        assert!(!is_call_id("call_01jzg6q9g3s8v2x7k4m1n5p0rt"), "lowercase");
        assert!(!is_call_id("run_01JZ6Q9G3S8V2X7K4M1N5P0R2T"));
        assert!(!is_call_id(""));
        // Run ids: the ULID shape and the legacy nanos-nonce shape.
        assert!(is_run_id("run_01JZ6Q9G3S8V2X7K4M1N5P0R2T"));
        assert!(is_run_id("run-1730000000000000000-7"));
        assert!(!is_run_id("run_bench00000001"), "a bench-shaped id");
        assert!(!is_run_id("call_01JZ6Q9G3S8V2X7K4M1N5P0R2T"));
        assert!(!is_run_id("runs"));
        assert!(!is_run_id("run-"));
        assert!(!is_run_id("run-a-b"));
    }

    #[test]
    fn the_cursor_wrappers_round_trip_and_refuse_foreign_kinds() {
        let id = format!("call_{}", crate::domain::mint_ulid());
        let cursor = encode_call_cursor(&id);
        assert_eq!(decode_call_cursor(&cursor).unwrap(), id);
        // A Run cursor decodes as a cursor but names no call id: refused.
        let run_cursor = crate::encode_cursor("run_01JZ6Q9G3S8V2X7K4M1N5P0R2T");
        let err = decode_call_cursor(&run_cursor).unwrap_err();
        assert_eq!(err.code(), "invalid-cursor");
        // A bare id, padding and garbage all refuse with the typed code.
        for bad in ["call_01JZ6Q9G3S8V2X7K4M1N5P0R2T", "nonsense", ""] {
            assert_eq!(
                decode_call_cursor(bad).unwrap_err().code(),
                "invalid-cursor",
                "{bad:?}"
            );
        }
        // The Run listing's codec is reused verbatim: the encodings agree.
        assert_eq!(cursor, crate::encode_cursor(&id));
    }

    #[test]
    fn the_model_mask_hides_credential_shapes_and_foreign_bytes() {
        assert_eq!(expose_model("gpt-4o"), "gpt-4o");
        assert_eq!(expose_model("claude-3-5-sonnet"), "claude-3-5-sonnet");
        // The exact shape the credential parser knows is masked — the full
        // prefix plus the exact body length. A shorter fragment is not the
        // known shape and is shown as recorded.
        for prefix in ["hk_run_", "hk_ref_", "hk_ann_", "hk_pxy_"] {
            let value = format!("{prefix}{}", "A".repeat(52));
            assert_eq!(expose_model(&value), MODEL_REDACTED, "{value}");
        }
        assert_eq!(expose_model("hk_run_ABCDEFGH"), "hk_run_ABCDEFGH");
        // Another provider's shape is NOT recognised: shown as recorded.
        assert_eq!(expose_model("sk-live-abc123"), "sk-live-abc123");
        // Bytes outside visible ASCII become `?` (a tampered file).
        assert_eq!(expose_model("gpt\x004o"), "gpt?4o");
        assert_eq!(expose_model("модель"), "??????");
        assert_eq!(expose_model("a\tb"), "a?b");
    }

    #[test]
    fn the_query_clamps_its_limit_and_the_document_echoes_its_filters() {
        assert_eq!(CallsQuery::all().clamped_limit(), CALLS_DEFAULT_LIMIT);
        let mut query = CallsQuery::all();
        query.limit = 0;
        assert_eq!(query.clamped_limit(), CALLS_MAX_LIMIT);
        query.limit = CALLS_MAX_LIMIT + 1;
        assert_eq!(query.clamped_limit(), CALLS_MAX_LIMIT);
        assert_eq!(CALLS_MAX_LIMIT, crate::ports::PAGE_MAX);
        // The document echoes the applied filters and omits the absent.
        let listing = CallListing {
            items: Vec::new(),
            next_cursor: None,
            scan_limited: false,
            searched_back_to: None,
            skipped_segments: 0,
            skipped_files: Vec::new(),
        };
        let document = listing.into_document(CallFilters {
            run: Some("run_01JZ6Q9G3S8V2X7K4M1N5P0R2T".to_string()),
            view: CallView::Problems,
            limit: 2,
            ..CallFilters::default()
        });
        assert_eq!(document.schema_version, CALLS_LISTING_SCHEMA_VERSION);
        let value = serde_json::to_value(&document).unwrap();
        assert_eq!(value["filters"]["run"], "run_01JZ6Q9G3S8V2X7K4M1N5P0R2T");
        assert_eq!(value["filters"]["view"], "problems");
        assert_eq!(value["filters"]["limit"], 2);
        assert!(value["filters"].get("outcome").is_none());
        assert!(value["next_cursor"].is_null());
        assert!(!value["scan_limited"].as_bool().unwrap());
    }

    #[test]
    fn the_view_wire_forms_round_trip() {
        assert_eq!(CallView::All.as_str(), "all");
        assert_eq!(CallView::Problems.as_str(), "problems");
        assert_eq!(CallView::from_wire("all"), Some(CallView::All));
        assert_eq!(CallView::from_wire("problems"), Some(CallView::Problems));
        assert!(CallView::from_wire("everything").is_none());
        assert_eq!(CallView::default(), CallView::All);
    }

    #[test]
    fn the_error_codes_are_the_stable_wire_strings() {
        assert_eq!(
            CallError::InvalidCursor { detail: "d".into() }.code(),
            "invalid-cursor"
        );
        assert_eq!(
            CallError::InvalidCallId {
                call_id: "x".into()
            }
            .code(),
            "invalid-call-id"
        );
        assert_eq!(
            CallError::InvalidRun { value: "x".into() }.code(),
            "invalid-run"
        );
        assert_eq!(
            CallError::InvalidTime { detail: "d".into() }.code(),
            "invalid-time"
        );
        assert_eq!(
            CallError::InvalidFilter {
                param: "outcome",
                detail: "d".into()
            }
            .code(),
            "invalid-filter"
        );
        assert_eq!(
            CallError::NotFound {
                call_id: "x".into()
            }
            .code(),
            "call-not-found"
        );
        assert_eq!(
            CallError::Unavailable { detail: "d".into() }.code(),
            "calls-unavailable"
        );
    }
}
