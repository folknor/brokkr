//! A profiled run's hotpath report (`docs/commands/library.md`, Profiling a
//! scenario): where it goes, and what makes it a profile of the requested
//! dimension rather than a file that merely parses.
//!
//! The report is the hotpath crate's own JSON, not a protocol document: it
//! carries no `protocol` field and is not read by the strict reader. It is
//! read once the launch is released, without following a link, and judged
//! by the rule here before [`crate::db::hotpath_data_from_json`] converts it,
//! since that conversion accepts anything with a section in it.

use std::path::Path;

use crate::db::{HotpathData, Quantity};
use crate::measure::ProfiledMode;

/// The report's name in the run directory, beside the request.
pub(crate) const REPORT_FILE: &str = "hotpath.json";

/// The section a mode's profile is in.
fn section(mode: ProfiledMode) -> &'static str {
    match mode {
        ProfiledMode::Hotpath => "functions_timing",
        ProfiledMode::Alloc => "functions_alloc",
    }
}

/// Read and judge the report at `path` for `mode`.
pub(crate) fn read(path: &Path, mode: ProfiledMode) -> Result<HotpathData, String> {
    let bytes = match super::artefact::entry_kind(path)? {
        None => return Err(format!("the harness wrote no {REPORT_FILE}")),
        Some(_) => super::artefact::read_regular(path, super::artefact::FILE_LIMIT)?,
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("{REPORT_FILE} is not JSON: {e}"))?;
    judge(&value, mode)?;
    crate::db::hotpath_data_from_json(&value)
        .ok_or_else(|| format!("{REPORT_FILE} holds no profile"))
}

/// The validity rule: the mode's section has a `data` array; every entry is
/// an object with a nonempty `name` and a `calls` count in 0 to 2^63-1; at
/// least one entry was called and has a nonnegative `total` of the mode's
/// dimension (a duration for timing, a byte size for allocation). Entries
/// counted without a total are kept beside it.
pub(crate) fn judge(value: &serde_json::Value, mode: ProfiledMode) -> Result<(), String> {
    let name = section(mode);
    let data = value
        .as_object()
        .ok_or_else(|| format!("{REPORT_FILE} is not a JSON object"))?
        .get(name)
        .and_then(|s| s.get("data"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("{REPORT_FILE} has no {name} profile"))?;
    let mut measured = false;
    for (i, entry) in data.iter().enumerate() {
        let at = format!("{REPORT_FILE} {name}.data[{i}]");
        let obj = entry
            .as_object()
            .ok_or_else(|| format!("{at} is not an object"))?;
        if !obj
            .get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|n| !n.is_empty())
        {
            return Err(format!("{at} has no name"));
        }
        let calls = obj
            .get("calls")
            .and_then(serde_json::Value::as_i64)
            .filter(|c| *c >= 0)
            .ok_or_else(|| format!("{at} has no call count in 0 to 2^63-1"))?;
        // The sign is read off the spelling: the parse rounds to whole
        // canonical units, so `-0.1 ns` would pass as zero.
        let total = obj
            .get("total")
            .and_then(serde_json::Value::as_str)
            .filter(|t| !t.trim_start().starts_with('-'))
            .and_then(Quantity::parse);
        let of_mode = match (mode, total) {
            (ProfiledMode::Hotpath, Some(Quantity::DurationNs(v)))
            | (ProfiledMode::Alloc, Some(Quantity::Bytes(v))) => v >= 0,
            _ => false,
        };
        measured |= calls >= 1 && of_mode;
    }
    if measured {
        Ok(())
    } else {
        Err(format!(
            "{REPORT_FILE} has no called function with a {} total in {name}",
            match mode {
                ProfiledMode::Hotpath => "duration",
                ProfiledMode::Alloc => "byte",
            }
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn judged(text: &str, mode: ProfiledMode) -> Result<(), String> {
        let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        judge(&value, mode)
    }

    #[test]
    fn a_profile_needs_a_called_function_of_its_dimension() {
        let timing = r#"{"functions_timing": {"data": [
            {"name": "decode", "calls": 3, "total": "2 ms"},
            {"name": "idle", "calls": 0}]}}"#;
        assert!(judged(timing, ProfiledMode::Hotpath).is_ok());
        assert!(
            judged(timing, ProfiledMode::Alloc)
                .unwrap_err()
                .contains("no functions_alloc profile")
        );
        let alloc = r#"{"functions_timing": {"data": []},
            "functions_alloc": {"data": [{"name": "decode", "calls": 1, "total": "0 B"}]}}"#;
        assert!(
            judged(alloc, ProfiledMode::Alloc).is_ok(),
            "zero bytes is a measurement"
        );
        for bad in [
            r#"{"functions_timing": {"data": [null]}}"#,
            r#"{"functions_timing": {"data": [{"name": "anything"}]}}"#,
            r#"{"functions_timing": {"data": [{"name": "f", "calls": 1, "total": "-1 ms"}]}}"#,
            r#"{"functions_timing": {"data": [{"name": "f", "calls": 1, "total": "-0.1 ns"}]}}"#,
            r#"{"functions_timing": {"data": [{"name": "f", "calls": 1, "total": "2 KB"}]}}"#,
            r#"{"functions_timing": {"data": [{"name": "f", "calls": 9223372036854775808, "total": "1 ms"}]}}"#,
            r#"{"functions_timing": {"data": [{"name": "", "calls": 1, "total": "1 ms"}]}}"#,
            r#"{"functions_timing": {"data": [{"name": "f", "calls": 0, "total": "1 ms"}]}}"#,
            r#"{"threads": {"data": []}}"#,
            "[]",
        ] {
            assert!(judged(bad, ProfiledMode::Hotpath).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_missing_or_linked_report_is_refused() {
        let root = crate::test_scratch::scratch("pbfhogg-library-profile", "read");
        let path = root.join(REPORT_FILE);
        assert!(
            read(&path, ProfiledMode::Hotpath)
                .err()
                .is_some_and(|e| e.contains("wrote no"))
        );
        let real = root.join("real.json");
        std::fs::write(
            &real,
            r#"{"functions_timing": {"data": [{"name": "f", "calls": 1, "total": "1 ms"}]}}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        assert!(
            read(&path, ProfiledMode::Hotpath).is_err(),
            "never followed"
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&real, &path).unwrap();
        let Ok(data) = read(&path, ProfiledMode::Hotpath) else {
            panic!("a regular report is read");
        };
        assert_eq!(data.functions[0].name, "f");
    }
}
