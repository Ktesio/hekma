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

/// Current time as whole seconds since the Unix epoch.
///
/// The companion of [`now_rfc3339`] for callers that need arithmetic on the
/// instant (the Run listing's ended-Run window: `now − N days`, formatted
/// back through [`format_rfc3339`]). Clamps to the epoch like its sibling.
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current time as an RFC 3339 UTC string, e.g. `2026-07-03T14:05:09Z`.
///
/// Uses the system clock. Times before the Unix epoch (clock set wildly in the
/// past) clamp to the epoch rather than panicking — this is state metadata, not
/// a correctness-critical value.
pub fn now_rfc3339() -> String {
    format_rfc3339(now_unix_secs())
}

/// Current time as an RFC 3339 UTC string with microseconds, e.g.
/// `2026-07-03T14:05:09.123456Z` (story 23-1: per-call records carry
/// sub-second start and end times).
pub fn now_rfc3339_micros() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let whole = format_rfc3339(now.as_secs());
    let stem = whole.trim_end_matches('Z');
    format!("{stem}.{:06}Z", now.subsec_micros())
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

/// Format an instant as RFC 3339 UTC with exactly six fractional digits
/// (the per-call records' stored timestamp shape, so a TEXT comparison of
/// two such strings is the chronological comparison).
pub fn format_rfc3339_micros(unix_secs: u64, micros: u32) -> String {
    let whole = format_rfc3339(unix_secs);
    let stem = whole.trim_end_matches('Z');
    format!("{stem}.{micros:06}Z")
}

/// Parse an RFC 3339 UTC instant into `(unix seconds, microseconds)`.
///
/// Accepts `YYYY-MM-DDTHH:MM:SS` with an optional fractional part of up to
/// nine digits (anything past six is truncated — the stored shape keeps
/// microseconds) and the `Z`, `z`, `+00:00` or `-00:00` zone. Every other
/// offset is refused: the engine stores UTC only, and silently rewriting a
/// non-UTC zone would shift a window the caller meant literally. `T` may be
/// a space (the RFC's alternative separator). Returns `None` on any other
/// shape — callers turn that into their typed refusal.
pub fn parse_rfc3339(raw: &str) -> Option<(u64, u32)> {
    let bytes = raw.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let (date, time) = (raw.get(0..10)?, raw.get(11..)?);
    if bytes[4] != b'-' || bytes[7] != b'-' || !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }
    let digits = |text: &str| text.bytes().all(|b| b.is_ascii_digit());
    let year: u64 = date.get(0..4)?.parse().ok()?;
    let month: u64 = date.get(5..7)?.parse().ok()?;
    let day: u64 = date.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // The time-of-day: HH:MM:SS, then an optional fraction, then the zone.
    let hh: u64 = time.get(0..2)?.parse().ok()?;
    let mm: u64 = time.get(3..5)?.parse().ok()?;
    let ss: u64 = time.get(6..8)?.parse().ok()?;
    if !digits(&time[0..2]) || !digits(&time[3..5]) || !digits(&time[6..8]) {
        return None;
    }
    if time.as_bytes()[2] != b':' || time.as_bytes()[5] != b':' {
        return None;
    }
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let mut rest = &time[8..];
    let mut micros: u32 = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits_len = fraction
            .bytes()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits_len == 0 {
            return None; // a bare dot is not a fraction
        }
        let (digits_text, tail) = fraction.split_at(digits_len);
        let mut value: u64 = 0;
        for ch in digits_text.chars().take(6) {
            value = value * 10 + u64::from(ch.to_digit(10)?);
        }
        let places = digits_len.min(6);
        micros = (value * 10u64.pow((6 - places) as u32)) as u32;
        rest = tail;
    }
    match rest {
        "Z" | "z" | "+00:00" | "-00:00" => {}
        _ => return None,
    } // Reject February 30th and friends: the calendar round trip must agree.
    let days = days_from_civil(year, month, day)?;
    if civil_from_days(days) != (year, month, day) {
        return None;
    }
    let secs = days * 86_400 + hh * 3_600 + mm * 60 + ss;
    Some((secs, micros))
}

/// Convert a civil date (year, month, day) into days since the epoch —
/// Hinnant's `days_from_civil`, restricted to the non-negative range, with
/// a validity check (`None` on a date the calendar does not hold, so
/// February 30th refuses rather than silently normalising).
fn days_from_civil(year: u64, month: u64, day: u64) -> Option<u64> {
    if !(1..100_000).contains(&year) {
        return None;
    }
    let (year_adj, month_adj) = if month <= 2 {
        (year - 1, month + 12)
    } else {
        (year, month)
    };
    let era = (year_adj as i64) / 400;
    let yoe = (year_adj as i64) - era * 400;
    let doy = (153 * (month_adj as i64 - 3) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days).ok()
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

    #[test]
    fn the_parser_round_trips_the_stored_shapes() {
        // Whole seconds, the event shapes.
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some((0, 0)));
        assert_eq!(
            parse_rfc3339("2026-07-03T00:00:00Z"),
            Some((1_783_036_800, 0))
        );
        // Microseconds, the per-call record shape.
        assert_eq!(
            parse_rfc3339("2026-11-12T00:00:00.000001Z"),
            Some((1_794_441_600, 1))
        );
        assert_eq!(
            parse_rfc3339("2026-11-12T00:00:00.5Z"),
            Some((1_794_441_600, 500_000))
        );
        // Nine fractional digits truncate to microseconds; nanosecond
        // precision beyond that is not the stored shape.
        assert_eq!(
            parse_rfc3339("2026-11-12T00:00:00.123456789Z"),
            Some((1_794_441_600, 123_456))
        );
        // The alternative separators and zero zones.
        assert_eq!(
            parse_rfc3339("2026-07-03 00:00:00+00:00"),
            Some((1_783_036_800, 0))
        );
        assert_eq!(
            parse_rfc3339("2026-07-03t00:00:00z"),
            Some((1_783_036_800, 0))
        );
        // Leap-day shapes parse on the years that hold them.
        assert_eq!(
            parse_rfc3339("2024-02-29T12:34:56Z"),
            Some((1_709_210_096, 0))
        );
    }

    #[test]
    fn the_parser_refuses_shapes_it_cannot_hold() {
        for bad in [
            "",
            "not-a-time",
            "2026-11-12",                // no time
            "2026-11-12T00:00Z",         // no seconds
            "2026-11-12T00:00:00",       // no zone
            "2026-11-12T00:00:00+01:00", // a non-UTC zone is never rewritten
            "2026-13-01T00:00:00Z",      // month out of range
            "2026-00-01T00:00:00Z",
            "2026-11-00T00:00:00Z",
            "2026-11-32T00:00:00Z",
            "2026-02-30T00:00:00Z", // a date the calendar does not hold
            "2026-11-12T24:00:00Z", // hour out of range
            "2026-11-12T23:60:00Z",
            "2026-11-12T00:00:00.Z",     // a bare dot
            "2026-11-12T00:00:00.12x4Z", // a non-digit inside the fraction
            "0000-01-01T00:00:00Z",      // before the formattable range
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn micros_formatting_is_the_stored_shape() {
        assert_eq!(format_rfc3339_micros(0, 0), "1970-01-01T00:00:00.000000Z");
        assert_eq!(
            format_rfc3339_micros(1_794_441_600, 1),
            "2026-11-12T00:00:00.000001Z"
        );
        // The formatting and the parser are inverses on the stored shape.
        for (secs, micros) in [(0u64, 0u32), (1_794_441_600, 1), (4_294_967_296, 999_999)] {
            let text = format_rfc3339_micros(secs, micros);
            assert_eq!(parse_rfc3339(&text), Some((secs, micros)), "{text}");
        }
    }
}
