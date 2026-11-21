//! Per-call records of the shared Metering Proxy (story 23-1, spine AD-26):
//! the data a call-start and a call-close record carry to the calls store.
//!
//! A record never holds a request or response byte, a header value or a
//! token: identifiers, the Run's Control Depth, the binding name, the
//! disposition and outcome, the status, the model (capped and
//! charset-checked), token counts and timings.

use crate::domain::RunId;
use crate::ports::ProcessFingerprint;

/// The longest model name a record keeps (visible ASCII only).
pub(crate) const MAX_MODEL_CHARS: usize = 128;

/// What the proxy did with a call before forwarding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CallDisposition {
    /// Forwarded.
    Pass,
    /// Refused by a `block` reason on the Run or an ancestor.
    Block,
}

impl CallDisposition {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CallDisposition::Pass => "pass",
            CallDisposition::Block => "block",
        }
    }
}

/// How a call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CallOutcome {
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
}

impl CallOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CallOutcome::Completed => "completed",
            CallOutcome::Refused => "refused",
            CallOutcome::UpstreamError => "upstream-error",
            CallOutcome::ClientDisconnect => "client-disconnect",
            CallOutcome::Shutdown => "shutdown",
            CallOutcome::Crash => "crash",
            CallOutcome::Recovered => "recovered",
        }
    }
}

/// A call's status class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusClass {
    Success,
    Redirect,
    ClientError,
    ServerError,
    /// Refused by the proxy (no upstream status).
    Refused,
    /// No upstream status (connect, TLS or mid-body failure, disconnect).
    TransportError,
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

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            StatusClass::Success => "2xx",
            StatusClass::Redirect => "3xx",
            StatusClass::ClientError => "4xx",
            StatusClass::ServerError => "5xx",
            StatusClass::Refused => "refused",
            StatusClass::TransportError => "transport-error",
        }
    }
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

/// What a call-close record carries (a `calls` row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallRecord {
    pub call_id: String,
    pub run_id: RunId,
    pub token_id: Option<String>,
    pub control_depth: &'static str,
    pub binding: Option<String>,
    pub disposition: CallDisposition,
    pub outcome: CallOutcome,
    pub status: Option<u16>,
    pub status_class: StatusClass,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub started_at: String,
    pub ended_at: String,
    pub latency_us: u64,
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
}
