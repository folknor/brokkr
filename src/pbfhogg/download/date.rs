//! Calendar arithmetic and download-date formatting.

use std::path::Path;
use std::time::SystemTime;

use crate::civil_date::CivilDate;

/// Today's date as `YYYYMMDD`.
pub(super) fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    unix_to_yyyymmdd(i64::try_from(secs).unwrap_or(i64::MAX))
}

/// The UTC `YYYYMMDD` date containing a Unix timestamp. A timestamp outside
/// the representable calendar (never a real clock or mtime) formats as the
/// epoch.
pub(super) fn unix_to_yyyymmdd(unix_secs: i64) -> String {
    CivilDate::from_unix_timestamp(unix_secs)
        .map_or_else(|| "19700101".to_owned(), CivilDate::yyyymmdd)
}

/// If the snapshot key parses as a `YYYYMMDD` date string, convert it to
/// `YYYY-MM-DD` for writing as the snapshot's `download_date`. Returns `None`
/// if the key isn't an 8-digit date (e.g. `pre-refactor` or `staging-1`).
///
/// Used by `run_as_snapshot` to derive the snapshot's identity date from
/// its key when the key follows the common dated convention. Snapshots with
/// non-date keys fall back to today's date.
pub(super) fn snapshot_key_to_iso_date(key: &str) -> Option<String> {
    CivilDate::parse_yyyymmdd(key).map(CivilDate::iso)
}

/// Convert a `YYYY-MM-DD` `download_date` string into `YYYYMMDD`. Returns
/// `None` if the input doesn't match the expected format.
pub(super) fn iso_date_to_yyyymmdd(s: &str) -> Option<String> {
    CivilDate::parse_iso(s).map(CivilDate::yyyymmdd)
}

/// Get the unix mtime of a file in seconds since epoch, or 0 on any error.
#[allow(clippy::cast_possible_wrap)]
pub(super) fn file_mtime_unix(path: &Path) -> i64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Format a `YYYYMMDD` date as `YYYY-MM-DD`; anything that is not such a
/// date is returned unchanged.
pub(super) fn iso_date_today(yyyymmdd: &str) -> String {
    CivilDate::parse_yyyymmdd(yyyymmdd).map_or_else(|| yyyymmdd.to_owned(), CivilDate::iso)
}

/// Parse a `YYYY-MM-DD` string as a unix epoch second (UTC midnight).
/// Returns `None` if the format doesn't match.
pub(super) fn iso_date_parse_unix(s: &str) -> Option<i64> {
    CivilDate::parse_iso(s).and_then(|date| date.to_unix_seconds(0, 0, 0))
}
