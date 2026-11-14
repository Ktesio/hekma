//! Reconciliation ingestion (story 19-5, PRD FR-45, spine AD-8): a Host or
//! the Operator submits a Run's confirmed usage — totals for the Run and its
//! descendants, as a provider's invoice or a gateway states them. Each
//! submission is appended; nothing estimated is ever overwritten. The
//! newest submission is the Run's current reconciled figure, and its drift
//! against the estimated figure is computed on read. Budgets keep
//! enforcing on estimated figures.

use serde::{Deserialize, Serialize};

use super::{RunError, RunId, RunUsage};
use crate::domain::cost::{EstimateLabel, Micros};

/// The longest reference a submission may carry.
pub const RECONCILE_REFERENCE_MAX: usize = 128;

/// The longest note a submission may carry.
pub const RECONCILE_NOTE_MAX: usize = 512;

/// Who confirmed the figures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReconcileSource {
    /// The model provider (an invoice or usage export).
    Provider,
    /// The Host (its own gateway or accounting).
    Host,
}

impl ReconcileSource {
    /// The stable wire form.
    pub fn as_str(&self) -> &'static str {
        match self {
            ReconcileSource::Provider => "provider",
            ReconcileSource::Host => "host",
        }
    }

    /// Parse the wire form.
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw {
            "provider" => Some(ReconcileSource::Provider),
            "host" => Some(ReconcileSource::Host),
            _ => None,
        }
    }
}

/// One submission's figures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileRequest {
    /// The idempotency reference: a retry with the same reference and the
    /// same figures returns the stored submission.
    pub reference: String,
    /// Who confirmed the figures.
    pub source: ReconcileSource,
    /// Confirmed input tokens.
    pub input_tokens: u64,
    /// Confirmed output tokens.
    pub output_tokens: u64,
    /// Confirmed cached input tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// Confirmed cost in micro-dollars, when known (never derived here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<Micros>,
    /// A free-text note (an invoice id, a period).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ReconcileRequest {
    /// Refuse a reference or note out of shape, or a count past `i64`.
    pub fn validate(&self) -> Result<(), RunError> {
        let invalid = |field: &str, reason: String| RunError::Invalid {
            field: field.to_string(),
            reason,
        };
        let reference_ok = !self.reference.is_empty()
            && self.reference.len() <= RECONCILE_REFERENCE_MAX
            && self.reference.chars().all(|c| c.is_ascii_graphic());
        if !reference_ok {
            return Err(invalid(
                "reference",
                format!(
                    "a reference is 1 to {RECONCILE_REFERENCE_MAX} printable characters with no \
                     spaces"
                ),
            ));
        }
        if self
            .note
            .as_ref()
            .is_some_and(|note| note.chars().count() > RECONCILE_NOTE_MAX || note.contains('\0'))
        {
            return Err(invalid(
                "note",
                format!("a note is at most {RECONCILE_NOTE_MAX} characters"),
            ));
        }
        let max = i64::MAX as u64;
        for (field, value) in [
            ("input_tokens", Some(self.input_tokens)),
            ("output_tokens", Some(self.output_tokens)),
            ("cached_tokens", self.cached_tokens),
        ] {
            if value.is_some_and(|value| value > max) {
                return Err(invalid(field, "the count is too large".to_string()));
            }
        }
        if self.cost_micros.is_some_and(|cost| cost.get() < 0) {
            return Err(invalid(
                "cost",
                "a confirmed cost cannot be negative".to_string(),
            ));
        }
        if self
            .cached_tokens
            .is_some_and(|cached| cached > self.input_tokens)
        {
            return Err(invalid(
                "cached_tokens",
                "cached tokens are part of the input, so they cannot exceed it".to_string(),
            ));
        }
        Ok(())
    }
}

/// One stored submission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconciliation {
    /// The Run it reconciles.
    pub run_id: RunId,
    /// Its figures, as submitted.
    #[serde(flatten)]
    pub figures: ReconcileRequest,
    /// Who submitted it: `operator` or `host`.
    pub submitted_by: String,
    /// When it was accepted (RFC 3339).
    pub submitted_at: String,
    /// Always `reconciled` (AD-8).
    pub estimate: EstimateLabel,
}

/// The difference reconciled − estimated, per dimension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileDrift {
    /// Input tokens.
    pub input_tokens: i64,
    /// Output tokens.
    pub output_tokens: i64,
    /// Cached tokens, when both figures know them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<i64>,
    /// Micro-dollars, when the submission carried a confirmed cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<i64>,
}

/// A Run's current reconciled figure beside its estimated one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciledView {
    /// The newest submission.
    pub current: Reconciliation,
    /// How many submissions the Run has.
    pub submissions: u64,
    /// The drift of the newest submission against the estimated figure.
    pub drift: ReconcileDrift,
}

impl ReconciledView {
    /// The view of `current` against `estimated` (both cover the Run and
    /// its descendants).
    pub fn of(current: Reconciliation, submissions: u64, estimated: &RunUsage) -> Self {
        let signed = |value: u64| value.min(i64::MAX as u64) as i64;
        let figures = &current.figures;
        let drift = ReconcileDrift {
            input_tokens: signed(figures.input_tokens) - signed(estimated.input_tokens),
            output_tokens: signed(figures.output_tokens) - signed(estimated.output_tokens),
            cached_tokens: figures
                .cached_tokens
                .zip(estimated.cached_tokens)
                .map(|(confirmed, counted)| signed(confirmed) - signed(counted)),
            cost_micros: figures
                .cost_micros
                .map(|cost| cost.get().saturating_sub(estimated.cost_micros.get())),
        };
        ReconciledView {
            current,
            submissions,
            drift,
        }
    }
}

/// What a submission did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileOutcome {
    /// The stored submission.
    pub reconciliation: Reconciliation,
    /// `true` when it was a retry of a stored one (nothing new stored).
    pub replayed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ReconcileRequest {
        ReconcileRequest {
            reference: "inv-2026-11".to_string(),
            source: ReconcileSource::Provider,
            input_tokens: 1_000,
            output_tokens: 200,
            cached_tokens: Some(100),
            cost_micros: Some(Micros(5_000)),
            note: Some("November invoice".to_string()),
        }
    }

    #[test]
    fn a_request_validates_its_reference_note_and_counts() {
        assert!(request().validate().is_ok());
        for reference in ["", "has space", &"x".repeat(129)] {
            let mut bad = request();
            bad.reference = reference.to_string();
            assert!(bad.validate().is_err(), "{reference:?}");
        }
        let mut bad = request();
        bad.note = Some("n".repeat(513));
        assert!(bad.validate().is_err());
        let mut bad = request();
        bad.input_tokens = u64::MAX;
        assert!(bad.validate().is_err());
        let mut bad = request();
        bad.cost_micros = Some(Micros(-1));
        assert!(bad.validate().is_err());
        let mut bad = request();
        bad.cached_tokens = Some(2_000);
        assert!(bad.validate().is_err());
        assert_eq!(
            ReconcileSource::from_wire("host"),
            Some(ReconcileSource::Host)
        );
        assert_eq!(ReconcileSource::from_wire("nope"), None);
        assert_eq!(ReconcileSource::Provider.as_str(), "provider");
    }

    #[test]
    fn drift_is_reconciled_minus_estimated_and_absent_where_unknown() {
        let current = Reconciliation {
            run_id: RunId::from_wire("run_R".to_string()),
            figures: request(),
            submitted_by: "host".to_string(),
            submitted_at: "t".to_string(),
            estimate: EstimateLabel::Reconciled,
        };
        let estimated = RunUsage {
            input_tokens: 900,
            output_tokens: 250,
            cached_tokens: None,
            cost_micros: Micros(4_000),
            estimate: EstimateLabel::Estimated,
        };
        let view = ReconciledView::of(current.clone(), 2, &estimated);
        assert_eq!(view.drift.input_tokens, 100);
        assert_eq!(view.drift.output_tokens, -50);
        assert_eq!(view.drift.cached_tokens, None);
        assert_eq!(view.drift.cost_micros, Some(1_000));
        let mut uncosted = current;
        uncosted.figures.cost_micros = None;
        let view = ReconciledView::of(
            uncosted,
            1,
            &RunUsage {
                cached_tokens: Some(40),
                ..estimated
            },
        );
        assert_eq!(view.drift.cost_micros, None);
        assert_eq!(view.drift.cached_tokens, Some(60));
    }
}
