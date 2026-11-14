//! Retention and compaction (story 24-4, spine AD-27, PRD FR-49).
//!
//! One stored row holds two windows: how long an ended Run keeps its full
//! detail ([`RetentionSettings::run_retention_days`]) and how long per-call
//! records are kept ([`RetentionSettings::calls_retention_days`]). The Run
//! window drives BOTH the listing's ended-Run cutoff and the compaction
//! pass, so the two can never disagree. Past it, an ended Run with no open
//! descendant is reduced to a [`CompactedRun`]: its id, usage totals,
//! labels, end cause, Control Depth and timestamps. Its detail rows are
//! deleted; its ancestors' rollups, its events and its outbox rows stay.
//! The calls window drops whole calls-store segments.

use serde::{Deserialize, Serialize};

use super::Label;

/// The default of both windows, in days.
pub const RETENTION_DEFAULT_DAYS: u32 = 7;

/// The smallest window a setting may hold, in days.
pub const RETENTION_MIN_DAYS: u32 = 1;

/// The largest window a setting may hold, in days (about ten years).
pub const RETENTION_MAX_DAYS: u32 = 3650;

/// The most Runs one compaction job reduces (one writer job each, so a
/// pass never holds the writer long: about 30 ms at 100,000 Runs, inside
/// the acked-write bound a queued write may wait behind).
pub const COMPACTION_BATCH: usize = 250;

/// How often the sweeper runs a compaction pass (and once at start).
pub const COMPACTION_INTERVAL_SECS: u64 = 86_400;

/// The two retention windows and when they were last set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionSettings {
    /// Days an ended Run keeps its full detail before compaction.
    pub run_retention_days: u32,
    /// Days per-call records are kept before their file is deleted whole.
    pub calls_retention_days: u32,
    /// When the settings were last changed; `None` while both are defaults
    /// never set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        RetentionSettings {
            run_retention_days: RETENTION_DEFAULT_DAYS,
            calls_retention_days: RETENTION_DEFAULT_DAYS,
            updated_at: None,
        }
    }
}

/// What the compaction passes have done so far.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionState {
    /// When the last pass finished; `None` before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pass_at: Option<String>,
    /// Ended Runs compacted, all passes together.
    pub runs_compacted: u64,
    /// Calls-store files deleted whole, all passes together.
    pub segments_dropped: u64,
}

/// The settings and the compaction state together (`hekma run retention`
/// and `GET /v0/retention`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionView {
    /// The two windows.
    pub settings: RetentionSettings,
    /// What compaction has done.
    pub compaction: CompactionState,
}

/// A change to the windows: `None` leaves a window as it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionChange {
    /// The new Run window, in days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_retention_days: Option<u32>,
    /// The new calls window, in days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls_retention_days: Option<u32>,
}

impl RetentionChange {
    /// Refuse a window outside [`RETENTION_MIN_DAYS`]..=[`RETENTION_MAX_DAYS`],
    /// or a change that sets nothing.
    pub fn validate(&self) -> Result<(), RetentionError> {
        if self.run_retention_days.is_none() && self.calls_retention_days.is_none() {
            return Err(RetentionError::Invalid {
                detail: "name a window to change (the Run window, the calls window, or both)"
                    .to_string(),
            });
        }
        for days in [self.run_retention_days, self.calls_retention_days]
            .into_iter()
            .flatten()
        {
            if !(RETENTION_MIN_DAYS..=RETENTION_MAX_DAYS).contains(&days) {
                return Err(RetentionError::Invalid {
                    detail: format!(
                        "retention must be between {RETENTION_MIN_DAYS} and \
                         {RETENTION_MAX_DAYS} days (got {days})"
                    ),
                });
            }
        }
        Ok(())
    }
}

/// Why a retention read or change failed.
#[derive(Debug, thiserror::Error)]
pub enum RetentionError {
    /// A window outside the allowed range, or an empty change.
    #[error("{detail}")]
    Invalid {
        /// What was wrong.
        detail: String,
    },
    /// The store failed.
    #[error(transparent)]
    Store(#[from] crate::ports::StoreError),
}

/// What a compacted Run keeps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactedRun {
    /// The Run's id.
    pub run_id: String,
    /// Input tokens, the Run and its descendants together.
    pub input_tokens: u64,
    /// Output tokens, the Run and its descendants together.
    pub output_tokens: u64,
    /// Cached input tokens, the Run and its descendants together.
    pub cached_tokens: u64,
    /// The estimated cost in micro-dollars.
    pub cost_micros: i64,
    /// Its labels at compaction, with their sources.
    pub labels: Vec<Label>,
    /// Why it ended.
    pub end_cause: String,
    /// Its Control Depth (wire form).
    pub control_depth: String,
    /// When it was created.
    pub created_at: String,
    /// When it ended.
    pub ended_at: String,
    /// When it was compacted.
    pub compacted_at: String,
}

/// The ended-Run cutoff for a window of `days` before `now_unix_secs`, in
/// the RFC 3339 shape the rows store.
pub fn cutoff_for(now_unix_secs: u64, days: u32) -> String {
    crate::time::format_rfc3339(now_unix_secs.saturating_sub(u64::from(days) * 86_400))
}

/// A calls-store segment's date (its file stem, `YYYY-MM-DD`), when the
/// name is one.
pub(crate) fn segment_date(path: &std::path::Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let bytes = stem.as_bytes();
    let shaped = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
    shaped.then(|| stem.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_must_name_a_window_inside_the_bounds() {
        assert!(RetentionChange::default().validate().is_err());
        for bad in [0, 3651, u32::MAX] {
            let change = RetentionChange {
                run_retention_days: Some(bad),
                calls_retention_days: None,
            };
            let err = change.validate().unwrap_err().to_string();
            assert!(err.contains("between 1 and 3650 days"), "{err}");
            let change = RetentionChange {
                run_retention_days: None,
                calls_retention_days: Some(bad),
            };
            assert!(change.validate().is_err());
        }
        for good in [1, 7, 3650] {
            let change = RetentionChange {
                run_retention_days: Some(good),
                calls_retention_days: Some(good),
            };
            assert!(change.validate().is_ok());
        }
    }

    #[test]
    fn the_cutoff_and_segment_dates() {
        assert_eq!(cutoff_for(1_794_355_200, 7), "2026-11-04T00:00:00Z");
        assert_eq!(cutoff_for(0, 7), "1970-01-01T00:00:00Z");
        let date = |name: &str| segment_date(std::path::Path::new(name));
        assert_eq!(date("calls/2026-11-14.db").as_deref(), Some("2026-11-14"));
        assert_eq!(date("calls/2026-11-14-2.db"), None);
        assert_eq!(date("calls/notes.db"), None);
        assert_eq!(date("calls/2026x11-14.db"), None);
        assert_eq!(RetentionSettings::default().run_retention_days, 7);
    }
}
