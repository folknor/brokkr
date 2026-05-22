//! ccu-driven phases: `outdated` and `stale`.
//!
//! Shells out once to `ccu --json` (the user's check-updates tool) and
//! turns the response into two kinds of events:
//!
//! - `Outdated` for each check with non-null `severity`
//!   (patch/minor/major) - someone you can upgrade.
//! - `Stale` for each check whose `latest_released_at` is more than
//!   `STALE_DAYS` ago - either you're on the freshest version and it's
//!   old, or you're behind on an old crate. Either way, the project
//!   may not be actively maintained.
//!
//! Schema version pinned at 1 - the JSON contract is in
//! `~/Programs/check-updates/ccu`.
//!
//! All failure modes (tool missing, subprocess error, non-zero exit,
//! schema mismatch, parse error) collapse into a single `ToolMissing`
//! event covering both phases. The check is informational - if it
//! can't run, that shouldn't fail the whole `brokkr deps` invocation.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::{DepsEvent, OutdatedEvent, StaleEvent, ToolMissingEvent};

const TOOL: &str = "ccu";
const PHASES: &[&str] = &["outdated", "stale"];
const INSTALL_HINT: &str = "not installed; cargo install --path ~/Programs/check-updates/ccu";
const SUPPORTED_SCHEMA: u32 = 1;

/// ~8 months. Below this, a crate's latest release is "fresh enough"
/// and we say nothing.
const STALE_DAYS: i64 = 240;
/// ~2 years. Crosses from "stale" into the louder "abandoned" label.
const ABANDONED_DAYS: i64 = 730;

#[derive(Deserialize)]
struct CcuOutput {
    schema_version: u32,
    #[serde(default)]
    checks: Vec<CcuCheck>,
}

#[derive(Deserialize)]
struct CcuCheck {
    dependency: CcuDependency,
    installed: String,
    latest: String,
    /// `null` when up-to-date; otherwise `"patch"` / `"minor"` /
    /// `"major"`.
    severity: Option<String>,
    /// When the newest version on the registry was published. Used
    /// by the `stale` phase. ISO-8601 with timezone offset, verbatim
    /// from crates.io.
    #[serde(default)]
    latest_released_at: Option<String>,
}

#[derive(Deserialize)]
struct CcuDependency {
    name: String,
    source_file: String,
    line_number: u64,
}

pub fn run(project_root: &Path) -> Vec<DepsEvent> {
    match try_run(project_root) {
        Ok(events) => events,
        Err(reason) => vec![DepsEvent::ToolMissing(ToolMissingEvent {
            phase: PHASES[0],
            tool: TOOL,
            reason,
        })],
    }
}

fn try_run(project_root: &Path) -> Result<Vec<DepsEvent>, String> {
    let output = match Command::new(TOOL)
        .arg("--json")
        .current_dir(project_root)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
    {
        Ok(o) => o,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(INSTALL_HINT.to_string());
        }
        Err(err) => return Err(format!("spawn failed: {err}")),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output
            .status
            .code()
            .map_or("signal".to_string(), |c| c.to_string());
        let stderr_trimmed = stderr.trim();
        return Err(if stderr_trimmed.is_empty() {
            format!("exited with {code}")
        } else {
            format!("exited with {code}: {stderr_trimmed}")
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: CcuOutput = serde_json::from_str(&stdout)
        .map_err(|e| format!("could not parse --json output: {e}"))?;
    if parsed.schema_version != SUPPORTED_SCHEMA {
        return Err(format!(
            "schema_version={} but brokkr expects {SUPPORTED_SCHEMA}",
            parsed.schema_version
        ));
    }

    let now_jdn = now_julian_day();
    let mut events = Vec::new();
    // In multi-workspace projects ccu reports the same crate once per
    // workspace member that depends on it. The findings are identical
    // (same Cargo.lock, same registry data) - we only want to surface
    // each unique smell once.
    let mut seen_outdated: HashSet<(String, String, String)> = HashSet::new();
    let mut seen_stale: HashSet<(String, String)> = HashSet::new();
    for check in parsed.checks {
        if let Some(severity) = check.severity {
            let key = (
                check.dependency.name.clone(),
                check.installed.clone(),
                check.latest.clone(),
            );
            if seen_outdated.insert(key) {
                events.push(DepsEvent::Outdated(OutdatedEvent {
                    krate: check.dependency.name.clone(),
                    installed: check.installed.clone(),
                    latest: check.latest.clone(),
                    severity,
                    source_file: check.dependency.source_file.clone(),
                    line_number: check.dependency.line_number,
                }));
            }
        }
        if let Some(released_at) = &check.latest_released_at
            && let Some(age_days) = age_days_from(released_at, now_jdn)
            && let Some(severity) = classify_age(age_days)
            && seen_stale.insert((check.dependency.name.clone(), check.latest.clone()))
        {
            events.push(DepsEvent::Stale(StaleEvent {
                krate: check.dependency.name,
                version: check.latest,
                released_at: released_at.clone(),
                age_days: u64::try_from(age_days).unwrap_or(0),
                severity,
            }));
        }
    }
    Ok(events)
}

fn classify_age(age_days: i64) -> Option<&'static str> {
    if age_days >= ABANDONED_DAYS {
        Some("abandoned")
    } else if age_days >= STALE_DAYS {
        Some("stale")
    } else {
        None
    }
}

/// Returns days between `now` and the date encoded in `released_at`.
/// Accepts any ISO-8601-like string whose first 10 characters are
/// `YYYY-MM-DD`; the rest (time, fraction, timezone offset) is
/// ignored - day granularity is plenty for "older than 8 months".
fn age_days_from(released_at: &str, now_jdn: i64) -> Option<i64> {
    let (y, mo, d) = parse_iso_date(released_at)?;
    Some(now_jdn - julian_day(y, mo, d))
}

fn parse_iso_date(s: &str) -> Option<(i32, u32, u32)> {
    let bytes = s.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    let month = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    let day = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    Some((year, month, day))
}

/// Julian Day Number for a Gregorian date. Subtracting two JDNs gives
/// the day difference - no calendar math needed in the caller.
/// Formula from Richards (2013), via the Wikipedia "Julian day" article.
fn julian_day(y: i32, mo: u32, d: u32) -> i64 {
    let a = (14i64 - mo as i64) / 12;
    let yy = y as i64 + 4800 - a;
    let mm = mo as i64 + 12 * a - 3;
    d as i64 + (153 * mm + 2) / 5 + 365 * yy + yy / 4 - yy / 100 + yy / 400 - 32045
}

fn now_julian_day() -> i64 {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 2440588 is the JDN of 1970-01-01. as_secs() returns u64;
    // dividing by 86400 brings it well within i64 range for any
    // sane wall-clock value.
    2440588 + i64::try_from(now_secs / 86400).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso_with_offset() {
        assert_eq!(
            parse_iso_date("2024-11-12T18:34:21.123456+00:00"),
            Some((2024, 11, 12))
        );
    }

    #[test]
    fn parses_iso_with_zulu() {
        assert_eq!(parse_iso_date("2023-04-15T07:00:00Z"), Some((2023, 4, 15)));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_iso_date("nope").is_none());
        assert!(parse_iso_date("2023/04/15").is_none());
    }

    #[test]
    fn julian_day_matches_known_values() {
        // 1970-01-01 = JDN 2440588
        assert_eq!(julian_day(1970, 1, 1), 2440588);
        // 2000-01-01 = JDN 2451545
        assert_eq!(julian_day(2000, 1, 1), 2451545);
        // One year later
        assert_eq!(julian_day(2001, 1, 1) - julian_day(2000, 1, 1), 366);
    }

    #[test]
    fn classify_thresholds() {
        assert_eq!(classify_age(239), None);
        assert_eq!(classify_age(240), Some("stale"));
        assert_eq!(classify_age(729), Some("stale"));
        assert_eq!(classify_age(730), Some("abandoned"));
        assert_eq!(classify_age(3000), Some("abandoned"));
    }
}
