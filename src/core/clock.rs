//! ISO-8601 timestamps as strings, so stored times compare lexicographically
//! in memory and in SQL (the prototype's `isoTime` convention).

use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{Duration, OffsetDateTime};

const CANONICAL: &[time::format_description::BorrowedFormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// The current time as `2026-11-08T12:34:56.789Z`.
pub fn now_iso() -> String {
    format(OffsetDateTime::now_utc())
}

/// Formats a time in the canonical shape every stored timestamp uses.
pub fn format(time: OffsetDateTime) -> String {
    time.to_offset(time::UtcOffset::UTC)
        .format(CANONICAL)
        .unwrap_or_else(|_| "1970-01-01T00:00:00.000Z".to_string())
}

/// Parses an RFC 3339 timestamp; `None` for anything else.
pub fn parse(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).ok()
}

/// Normalizes a timestamp to the canonical shape, or returns it unchanged when
/// it does not parse (the prototype's `isoTime`).
pub fn normalize(value: &str) -> String {
    parse(value).map(format).unwrap_or_else(|| value.to_string())
}

/// `at` plus `ms` milliseconds, as a canonical timestamp.
pub fn plus_ms(at: &str, ms: i64) -> String {
    match parse(at) {
        Some(time) => format(time + Duration::milliseconds(ms)),
        None => at.to_string(),
    }
}

/// Milliseconds from `from` to `to`; 0 when either does not parse.
pub fn millis_between(from: &str, to: &str) -> i64 {
    match (parse(from), parse(to)) {
        (Some(a), Some(b)) => (b - a).whole_milliseconds() as i64,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_with_millis_and_z() {
        let at = "2026-09-23T00:00:05Z";
        assert_eq!(normalize(at), "2026-09-23T00:00:05.000Z");
        assert_eq!(plus_ms(at, 1500), "2026-09-23T00:00:06.500Z");
        assert_eq!(millis_between(at, "2026-09-23T00:01:05Z"), 60_000);
        assert_eq!(normalize("not a time"), "not a time");
    }
}
