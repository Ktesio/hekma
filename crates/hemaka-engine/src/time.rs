//! Minimal RFC 3339 UTC timestamp formatting.
//!
//! The spine mandates RFC 3339 UTC timestamps everywhere (events, ledger,
//! logs). No date/time crate is on the approved dependency list for this
//! story, and the need is tiny (format "now" as UTC), so this computes the
//! civil date from a Unix timestamp directly. When richer time handling is
//! needed (parsing, arithmetic), a future story can adopt a crate behind this
//! seam.
//!
//! Output shape: `YYYY-MM-DDTHH:MM:SSZ` (whole seconds, `Z` zone). This is a
//! valid RFC 3339 / ISO 8601 instant.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current time as an RFC 3339 UTC string, e.g. `2026-07-03T14:05:09Z`.
///
/// Uses the system clock. Times before the Unix epoch (clock set wildly in the
/// past) clamp to the epoch rather than panicking — this is state metadata, not
/// a correctness-critical value.
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_rfc3339(secs)
}

/// Format a count of whole seconds since the Unix epoch as RFC 3339 UTC.
///
/// Pure and total — separated from [`now_rfc3339`] so it can be unit-tested
/// against known epoch values without touching the clock.
pub fn format_rfc3339(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let secs_of_day = unix_secs % 86_400;
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert days-since-epoch (1970-01-01) into a `(year, month, day)` civil date.
///
/// Howard Hinnant's well-known `civil_from_days` algorithm, restricted to the
/// non-negative range (post-epoch), which is all this engine ever formats.
fn civil_from_days(days_since_epoch: u64) -> (u64, u64, u64) {
    // Shift epoch to 0000-03-01 to make leap handling uniform.
    let z = days_since_epoch + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_epoch() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn formats_known_instants() {
        // 2001-09-09T01:46:40Z — the famous 1_000_000_000 epoch second.
        assert_eq!(format_rfc3339(1_000_000_000), "2001-09-09T01:46:40Z");
        // 2026-07-03T00:00:00Z.
        assert_eq!(format_rfc3339(1_783_036_800), "2026-07-03T00:00:00Z");
        // A leap day: 2024-02-29T12:34:56Z.
        assert_eq!(format_rfc3339(1_709_210_096), "2024-02-29T12:34:56Z");
    }

    #[test]
    fn now_has_rfc3339_shape() {
        let now = now_rfc3339();
        // Length "YYYY-MM-DDTHH:MM:SSZ" == 20.
        assert_eq!(now.len(), 20, "{now}");
        assert!(now.ends_with('Z'), "{now}");
        assert_eq!(now.as_bytes()[4], b'-');
        assert_eq!(now.as_bytes()[7], b'-');
        assert_eq!(now.as_bytes()[10], b'T');
        assert_eq!(now.as_bytes()[13], b':');
        assert_eq!(now.as_bytes()[16], b':');
        // Year should be in a sane modern range (sanity, not exactness).
        let year: u32 = now[0..4].parse().unwrap();
        assert!((2020..2100).contains(&year), "{now}");
    }
}
