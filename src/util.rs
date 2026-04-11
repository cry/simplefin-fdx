use std::time::{SystemTime, UNIX_EPOCH};

/// Returns the current time as a Unix timestamp (seconds since epoch).
///
/// Panics only if the system clock is set before the Unix epoch — not a
/// realistic concern, but surfaced loudly so misconfigured environments fail
/// fast instead of silently returning 0.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_secs() as i64
}

/// Format a unix timestamp as a `YYYY-MM-DD` date string (UTC).
///
/// Returns `"1970-01-01"` for timestamps that cannot be represented.
pub fn unix_to_date_str(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).unwrap())
        .format("%Y-%m-%d")
        .to_string()
}

/// Parse a `YYYY-MM-DD` date string to a unix timestamp (midnight UTC).
///
/// Returns `0` for dates that cannot be parsed.
pub fn date_str_to_unix(date: &str) -> i64 {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
        .unwrap_or(0)
}
