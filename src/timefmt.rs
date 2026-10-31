//! Calendar formatting without a date library: Python `datetime.isoformat()`
//! for `updated_at` and `time.strftime('%Y-%m-%dT%H:%M:%SZ', gmtime())`.

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant).
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn split_seconds(seconds: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = seconds.div_euclid(86400);
    let rest = seconds.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        (rest / 3600) as u32,
        ((rest % 3600) / 60) as u32,
        (rest % 60) as u32,
    )
}

/// `time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(seconds))`
pub fn gmtime_z(seconds: i64) -> String {
    let (y, mo, d, h, mi, s) = split_seconds(seconds);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// `datetime.isoformat()` of an aware timestamp: local wall time for the
/// given UTC offset, microseconds only when non-zero, `+HH:MM[:SS]` offset.
pub fn isoformat(epoch_micros: i64, offset_seconds: i32) -> String {
    let local = epoch_micros + i64::from(offset_seconds) * 1_000_000;
    let (y, mo, d, h, mi, s) = split_seconds(local.div_euclid(1_000_000));
    let micros = local.rem_euclid(1_000_000);
    let mut out = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}");
    if micros != 0 {
        out.push_str(&format!(".{micros:06}"));
    }
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let offset = offset_seconds.unsigned_abs();
    out.push_str(&format!(
        "{sign}{:02}:{:02}",
        offset / 3600,
        (offset % 3600) / 60
    ));
    if !offset.is_multiple_of(60) {
        out.push_str(&format!(":{:02}", offset % 60));
    }
    out
}

/// RFC 7231 date for `Last-Modified`.
pub fn http_date(seconds: i64) -> String {
    let (y, mo, d, h, mi, s) = split_seconds(seconds);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]
        [seconds.div_euclid(86400).rem_euclid(7) as usize];
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][mo as usize - 1];
    format!("{weekday}, {d:02} {month} {y:04} {h:02}:{mi:02}:{s:02} GMT")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_round_trip() {
        for days in [-719468, -1, 0, 1, 11016, 20000, 2932896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(gmtime_z(0), "1970-01-01T00:00:00Z");
        assert_eq!(gmtime_z(1_761_955_200), "2025-11-01T00:00:00Z");
        assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
    }

    #[test]
    fn isoformat_like_python() {
        assert_eq!(
            isoformat(1_761_955_200_000_000, 0),
            "2025-11-01T00:00:00+00:00"
        );
        assert_eq!(
            isoformat(1_761_955_200_000_123, 9 * 3600),
            "2025-11-01T09:00:00.000123+09:00"
        );
        assert_eq!(
            isoformat(0, -(5 * 3600 + 30 * 60)),
            "1969-12-31T18:30:00-05:30"
        );
    }
}
