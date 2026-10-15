//! UTC timestamps for the piners corpus run store, using brokkr's shared
//! civil-date conversion so brokkr needs no date crate.
//!
//! [`now_sqlite_utc`] writes `YYYY-MM-DD hh:mm:ss`, what SQLite's
//! `datetime('now')` writes - the run store's `started_at`, kept so rows
//! recorded before brokkr stamped the start itself sort and compare with
//! newer ones.

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
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = i64::try_from(secs)
        .ok()
        .and_then(crate::civil_date::CivilDate::from_unix_timestamp)
        .unwrap_or(crate::civil_date::CivilDate::UNIX_EPOCH)
        .components();
    (i64::from(year), month, day, hh, mm, ss)
}

fn format_sqlite_utc(secs: u64) -> String {
    let (year, month, day, hh, mm, ss) = broken_down(secs);
    format!("{year:04}-{month:02}-{day:02} {hh:02}:{mm:02}:{ss:02}")
}

/// Current UTC time as `YYYY-MM-DD hh:mm:ss`, the corpus run store's
/// `started_at` format (SQLite's `datetime('now')`).
pub fn now_sqlite_utc() -> String {
    format_sqlite_utc(now_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn components_for_epoch_day(days: i64) -> Option<(i32, u32, u32)> {
        days.checked_mul(86_400)
            .and_then(crate::civil_date::CivilDate::from_unix_timestamp)
            .map(crate::civil_date::CivilDate::components)
    }

    #[test]
    fn civil_epoch() {
        assert_eq!(components_for_epoch_day(0), Some((1970, 1, 1)));
    }

    #[test]
    fn civil_leap_day() {
        // 2024-02-29 is day 19_782 since the epoch.
        assert_eq!(components_for_epoch_day(19_782), Some((2024, 2, 29)));
        assert_eq!(components_for_epoch_day(19_783), Some((2024, 3, 1)));
    }

    #[test]
    fn civil_2000_03_01() {
        // 2000 is a leap year (divisible by 400): 2000-03-01 is day 11_017.
        assert_eq!(components_for_epoch_day(11_017), Some((2000, 3, 1)));
        assert_eq!(components_for_epoch_day(11_016), Some((2000, 2, 29)));
    }

    #[test]
    fn civil_pre_epoch() {
        assert_eq!(components_for_epoch_day(-1), Some((1969, 12, 31)));
        assert_eq!(components_for_epoch_day(-365), Some((1969, 1, 1)));
    }

    #[test]
    fn fixed_instant_formats() {
        // 2024-02-29 13:45:09 UTC.
        let secs = 19_782 * 86_400 + 13 * 3600 + 45 * 60 + 9;
        assert_eq!(format_sqlite_utc(secs), "2024-02-29 13:45:09");
    }

    #[test]
    fn sqlite_shape() {
        let s = now_sqlite_utc();
        let b = s.as_bytes();
        assert_eq!(s.len(), 19, "{s}");
        assert_eq!(
            (b[4], b[7], b[10], b[13], b[16]),
            (b'-', b'-', b' ', b':', b':')
        );
    }
}
