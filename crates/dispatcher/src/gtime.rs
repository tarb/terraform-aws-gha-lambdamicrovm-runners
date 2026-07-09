//! GitHub timestamp parsing: strict `%Y-%m-%dT%H:%M:%SZ`, UTC.

use crate::clock::Epoch;
use jiff::fmt::strtime;
use jiff::tz::TimeZone;

const FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

#[derive(Debug, Clone, thiserror::Error)]
#[error("time data {0:?} does not match format \"%Y-%m-%dT%H:%M:%SZ\"")]
pub struct GhTimeError(pub String);

/// Parse `YYYY-MM-DDTHH:MM:SSZ` to epoch seconds (UTC).
pub fn parse_gh_time(s: &str) -> Result<Epoch, GhTimeError> {
    let err = || GhTimeError(s.to_string());
    let civil = strtime::parse(FORMAT, s)
        .and_then(|b| b.to_datetime())
        .map_err(|_| err())?;
    let zoned = civil.to_zoned(TimeZone::UTC).map_err(|_| err())?;
    Ok(Epoch(zoned.timestamp().as_second() as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reference_timestamps() {
        // reference values from CPython calendar.timegm(strptime(...))
        assert_eq!(parse_gh_time("1970-01-01T00:00:00Z").unwrap().0, 0.0);
        assert_eq!(
            parse_gh_time("2099-01-01T00:00:00Z").unwrap().0,
            4_070_908_800.0
        );
        assert_eq!(
            parse_gh_time("2026-01-02T03:04:05Z").unwrap().0,
            1_767_323_045.0
        );
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(parse_gh_time("2026-01-02 03:04:05Z").is_err());
        assert!(parse_gh_time("2026-01-02T03:04:05").is_err());
        assert!(parse_gh_time("not-a-time").is_err());
        assert!(parse_gh_time("2026-13-02T03:04:05Z").is_err());
    }
}
