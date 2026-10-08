//! UTC timestamps for the piners run stores, computed from the epoch with the
//! civil-date algorithm so brokkr needs no date crate.
//!
//! Two formats, one per store, and each store keeps its own:
//!
//! - [`now_rfc3339`]: `YYYY-MM-DDThh:mm:ssZ`, the lint store's format
//!   (`tv_anchored_at` and the lint run store's `started_at`).
//! - [`now_sqlite_utc`]: `YYYY-MM-DD hh:mm:ss`, what SQLite's `datetime('now')`
//!   writes - the corpus run store's `started_at`, kept so rows recorded before
//!   brokkr stamped the start itself sort and compare with newer ones.
//!
//! Unifying them would break string ordering against rows already stored (`T`
//! sorts after the space, and the trailing `Z` differs), so neither changes.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the epoch, or 0 if the clock reads before it.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Split epoch seconds into `(year, month, day, hh, mm, ss)`.
fn broken_down(secs: u64) -> (i64, u32, u32, u64, u64, u64) {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(i64::try_from(days).unwrap_or(0));
    (year, month, day, hh, mm, ss)
}

fn format_rfc3339(secs: u64) -> String {
    let (year, month, day, hh, mm, ss) = broken_down(secs);
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn format_sqlite_utc(secs: u64) -> String {
    let (year, month, day, hh, mm, ss) = broken_down(secs);
    format!("{year:04}-{month:02}-{day:02} {hh:02}:{mm:02}:{ss:02}")
}

/// Current UTC time as an RFC3339 string (`YYYY-MM-DDThh:mm:ssZ`), the lint
/// store's format.
pub fn now_rfc3339() -> String {
    format_rfc3339(now_secs())
}

/// Current UTC time as `YYYY-MM-DD hh:mm:ss`, the corpus run store's
/// `started_at` format (SQLite's `datetime('now')`).
pub fn now_sqlite_utc() -> String {
    format_sqlite_utc(now_secs())
}

/// Howard Hinnant's days-from-civil inverse: epoch-day count -> (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 { y + 1 } else { y },
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn civil_leap_day() {
        // 2024-02-29 is day 19_782 since the epoch.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
    }

    #[test]
    fn civil_2000_03_01() {
        // 2000 is a leap year (divisible by 400): 2000-03-01 is day 11_017.
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn civil_pre_epoch() {
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(-365), (1969, 1, 1));
    }

    #[test]
    fn fixed_instant_formats() {
        // 2024-02-29 13:45:09 UTC.
        let secs = 19_782 * 86_400 + 13 * 3600 + 45 * 60 + 9;
        assert_eq!(format_rfc3339(secs), "2024-02-29T13:45:09Z");
        assert_eq!(format_sqlite_utc(secs), "2024-02-29 13:45:09");
    }

    #[test]
    fn rfc3339_shape() {
        let s = now_rfc3339();
        let b = s.as_bytes();
        assert_eq!(s.len(), 20, "{s}");
        assert_eq!((b[4], b[7], b[10], b[13], b[16], b[19]), (b'-', b'-', b'T', b':', b':', b'Z'));
    }

    #[test]
    fn sqlite_shape() {
        let s = now_sqlite_utc();
        let b = s.as_bytes();
        assert_eq!(s.len(), 19, "{s}");
        assert_eq!((b[4], b[7], b[10], b[13], b[16]), (b'-', b'-', b' ', b':', b':'));
    }
}
