//! GitHub timestamp parsing.
//!
//! The Python dispatcher parses `%Y-%m-%dT%H:%M:%SZ` with
//! `time.mktime(time.strptime(...))` — mktime treats the struct as *local*
//! time, which equals UTC on Lambda (TZ=UTC). We parse as UTC directly.

use crate::pyfmt::PyErr;

const FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

/// Parse `YYYY-MM-DDTHH:MM:SSZ` to epoch seconds (UTC).
pub fn parse_gh_time(s: &str) -> Result<i64, PyErr> {
    let err = || PyErr::value_error(format!("time data '{s}' does not match format '{FORMAT}'"));
    let b = s.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return Err(err());
    }
    let num = |r: std::ops::Range<usize>| -> Result<i64, PyErr> {
        s.get(r).and_then(|t| t.parse::<i64>().ok()).ok_or_else(err)
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 61 {
        return Err(err());
    }
    Ok(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec)
}

/// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
fn days_from_civil(mut y: i64, m: i64, d: i64) -> i64 {
    if m <= 2 {
        y -= 1;
    }
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reference_timestamps() {
        // reference values from CPython calendar.timegm(strptime(...))
        assert_eq!(parse_gh_time("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(
            parse_gh_time("2099-01-01T00:00:00Z").unwrap(),
            4_070_908_800
        );
        assert_eq!(
            parse_gh_time("2026-01-02T03:04:05Z").unwrap(),
            1_767_323_045
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
