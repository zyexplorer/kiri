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

#[cfg(unix)]
/// A filter's time as Docker's `timetypes.GetTimestamp` reads it (prune
/// `until`): a Go duration before `now` (`24h`, `1h30m`, `1.5h`), an RFC 3339
/// date or time, in the daemon's local zone when it names none
/// (`2026-01-02`, `2026-01-02T15:04`, `2026-01-02T15:04:05.5Z`), or Unix
/// seconds (`1700000000.5`). Unix seconds, or None if it is none of these.
pub fn docker_timestamp(v: &str, now: SystemTime) -> Option<f64> {
    let now_secs = now.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    if v != "0"
        && let Some(d) = go_duration(v)
    {
        return Some(now_secs - d);
    }
    let zoned = v.contains(['z', 'Z', '+']) || v.matches('-').count() == 3;
    if let Some(t) = rfc3339_like(v, zoned) {
        return Some(if zoned { t } else { t - local_offset(now) as f64 });
    }
    if v.contains('-') {
        return None;
    }
    // Unix seconds, with an optional fraction (strconv.ParseInt on each part).
    let (sec, frac) = v.split_once('.').map_or((v, None), |(a, b)| (a, Some(b)));
    let sec: i64 = sec.parse().ok()?;
    let frac = match frac {
        Some(f) => {
            let n: i64 = f.parse().ok()?;
            n as f64 / 10f64.powi(f.len() as i32)
        }
        None => 0.0,
    };
    Some(sec as f64 + frac)
}

#[cfg(unix)]
/// Go's `time.ParseDuration`, in seconds: `[-+]` then number-unit pairs.
fn go_duration(s: &str) -> Option<f64> {
    let (neg, mut s) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    if s == "0" {
        return Some(0.0);
    }
    if s.is_empty() {
        return None;
    }
    let mut total = 0.0;
    while !s.is_empty() {
        let n = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
        let (num, rest) = s.split_at(n);
        if num.is_empty() || num == "." || num.matches('.').count() > 1 {
            return None;
        }
        let v: f64 = num.parse().ok()?;
        let u = rest.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(rest.len());
        let (unit, rest) = rest.split_at(u);
        total += v * match unit {
            "ns" => 1e-9,
            "us" | "\u{b5}s" | "\u{3bc}s" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        s = rest;
    }
    Some(if neg { -total } else { total })
}

#[cfg(unix)]
/// The date-and-time layouts GetTimestamp tries: `2006-01-02`, then
/// `T15`, `T15:04`, `T15:04:05` and a fraction (only after seconds), then
/// `Z` or `±07:00` when `zoned`. Unix seconds, before any local offset.
fn rfc3339_like(v: &str, zoned: bool) -> Option<f64> {
    let b = v.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> { v.get(r.clone()).filter(|x| x.bytes().all(|c| c.is_ascii_digit()) && x.len() == r.len())?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') || !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) {
        return None;
    }
    let mut i = 10;
    let (mut h, mut mi, mut se, mut frac, mut dot) = (0, 0, 0, 0.0, false);
    if b.get(i) == Some(&b'T') {
        h = num(i + 1..i + 3)?;
        i += 3;
        if b.get(i) == Some(&b':') && num(i + 1..i + 3).is_some() {
            mi = num(i + 1..i + 3)?;
            i += 3;
            if b.get(i) == Some(&b':') && num(i + 1..i + 3).is_some() {
                se = num(i + 1..i + 3)?;
                i += 3;
                if b.get(i) == Some(&b'.') {
                    let n = v[i + 1..].bytes().take_while(u8::is_ascii_digit).count();
                    if n == 0 {
                        return None;
                    }
                    frac = format!("0.{}", &v[i + 1..i + 1 + n]).parse().ok()?;
                    i += 1 + n;
                    dot = true;
                }
            }
        }
        if h > 23 || mi > 59 || se > 59 {
            return None;
        }
    }
    // A fraction only after seconds (Docker then uses RFC3339Nano).
    if v.contains('.') && !dot {
        return None;
    }
    let mut t = (days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se) as f64 + frac;
    let rest = &v[i..];
    match (zoned, rest.as_bytes()) {
        (false, []) | (true, [b'Z']) => {}
        (true, [sign @ (b'+' | b'-'), _, _, b':', _, _]) => {
            let (oh, om) = (num(i + 1..i + 3)?, num(i + 4..i + 6)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let off = (oh * 3600 + om * 60) as f64;
            t -= if *sign == b'+' { off } else { -off };
        }
        _ => return None,
    }
    Some(t)
}

#[cfg(unix)]
fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(unix)]
/// The daemon's UTC offset now, in seconds (Docker's
/// `time.FixedZone(reference.Zone())`).
fn local_offset(now: SystemTime) -> i64 {
    let secs = unix(now) as libc::time_t;
    // SAFETY: localtime_r fills the tm it is given and keeps no pointer.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() { 0 } else { tm.tm_gmtoff as i64 }
    }
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

    #[cfg(unix)]
    #[test]
    fn docker_timestamps() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let off = local_offset(now) as f64;
        let ts = |v: &str| docker_timestamp(v, now);
        assert_eq!(ts("24h"), Some(1_800_000_000.0 - 86_400.0));
        assert_eq!(ts("1h30m"), Some(1_800_000_000.0 - 5_400.0));
        assert_eq!(ts("1.5h"), Some(1_800_000_000.0 - 5_400.0));
        assert_eq!(ts("-10m"), Some(1_800_000_600.0));
        assert_eq!(ts("0"), Some(0.0));
        assert_eq!(ts("1700000000"), Some(1_700_000_000.0));
        assert_eq!(ts("1700000000.5"), Some(1_700_000_000.5));
        assert_eq!(ts("2024-02-29T12:00:00Z"), Some(1_709_208_000.0));
        assert_eq!(ts("2024-02-29T12:00:00.25Z"), Some(1_709_208_000.25));
        assert_eq!(ts("2024-02-29T12:00:00.0Z"), Some(1_709_208_000.0));
        assert_eq!(ts("2024-02-29T12:00:00+01:00"), Some(1_709_204_400.0));
        assert_eq!(ts("2024-02-29T12:00Z"), Some(1_709_208_000.0));
        assert_eq!(ts("2024-02-29T12Z"), Some(1_709_208_000.0));
        assert_eq!(ts("2024-02-29Z"), Some(1_709_164_800.0));
        assert_eq!(ts("2024-02-29"), Some(1_709_164_800.0 - off));
        assert_eq!(ts("2024-02-29T12:00:00"), Some(1_709_208_000.0 - off));
        assert_eq!(ts("2024-02-29T12"), Some(1_709_208_000.0 - off));
        for bad in ["", "1d", "yesterday", "2024-02-30", "2024-13-01", "2024-02-29T25", "2024-02-29T12:00.5Z", "2024-2-29", "12.x", "h", "1.2.3s"] {
            assert_eq!(ts(bad), None, "{bad}");
        }
    }
}
