//! RFC 3339 timestamps, the way the Docker API writes them, without a date library.

use std::time::{SystemTime, UNIX_EPOCH};

/// `2026-10-24T09:41:07.123456789Z`
pub fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, day) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:09}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_nanos()
    )
}

/// Docker's zero time, for containers that have not started or finished.
pub const ZERO: &str = "0001-01-01T00:00:00Z";

pub fn unix(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parses the date and time of an RFC 3339 string to Unix seconds, ignoring
/// fractions; offsets other than `Z` are applied. Returns 0 if unparseable.
pub fn parse_unix(s: &str) -> i64 {
    let n = |r: std::ops::Range<usize>| s.get(r).and_then(|v| v.parse::<i64>().ok());
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(se)) =
        (n(0..4), n(5..7), n(8..10), n(11..13), n(14..16), n(17..19))
    else {
        return 0;
    };
    let mut t = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se;
    if let Some(i) = s[19..].find(['+', '-']).map(|i| i + 19)
        && let (Some(oh), Some(om)) = (n(i + 1..i + 3), n(i + 4..i + 6))
    {
        let off = oh * 3600 + om * 60;
        t -= if &s[i..i + 1] == "+" { off } else { -off };
    }
    t
}

// Howard Hinnant's civil date algorithms.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn round_trips() {
        let t = UNIX_EPOCH + Duration::from_secs(1_792_800_000);
        let s = rfc3339(t);
        assert_eq!(parse_unix(&s), 1_792_800_000);
        assert_eq!(parse_unix("1970-01-01T00:00:00Z"), 0);
        assert_eq!(parse_unix("2024-02-29T12:00:00+01:00"), 1_709_204_400);
    }
}
