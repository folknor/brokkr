//! ccu-driven phases: `outdated` and `stale`.
//!
//! Shells out to `ccu --json` (the user's check-updates tool) and turns the
//! response into dependency-update and release-age events:
//!
//! - `Outdated` for each check with non-null `severity`
//!   (patch/minor/major) - someone you can upgrade.
//! - `Stale` for each check whose `latest_released_at` is more than
//!   `STALE_DAYS` ago - either you're on the freshest version and it's
//!   old, or you're behind on an old crate. Either way, the project
//!   may not be actively maintained.
//!
//! Schema version pinned at 1 - the JSON contract is the `ccu --json`
//! protocol.
//!
//! All failure modes (tool missing, subprocess error, deadline kill,
//! non-zero exit, schema mismatch, parse error) collapse into a shared
//! `ToolMissing` event covering the ccu-driven phases. The check is
//! informational - if it can't run, that shouldn't fail the whole
//! `brokkr deps` invocation.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize, Serializer};

use super::DepsEvent;
use super::phase::{Phase, PhaseOutcome};
use crate::error::DevError;
use crate::output;

const TOOL: &str = "ccu";
// `run()` consumes the entries in order, pairing them with its success results
// and emitting skipped outcomes for them on failure.
const PHASES: [Phase; 2] = [Phase::Outdated, Phase::Stale];
/// Wall-clock ceiling on the `ccu` call. It is a network client, and a
/// stalled registry connection otherwise hangs `brokkr deps` indefinitely.
/// Generous on purpose: a large workspace issues one registry query per
/// direct dep. Exceeding it degrades to the usual `ToolMissing` skip - the
/// phases are informational, so a timeout must never fail the run.
const CCU_DEADLINE: Duration = Duration::from_secs(120);
const INSTALL_HINT: &str = concat!(
    "not installed; install the check-updates `ccu` tool ",
    "and make it available on PATH"
);
/// Version of the external `ccu --json` protocol that this parser supports.
/// This is an integration contract, not a per-project setting.
const SUPPORTED_SCHEMA: u32 = 1;

/// Fixed policy age: below this, a crate's latest release is "fresh enough".
const STALE_DAYS: i64 = 240;
/// Fixed policy age at which a stale crate is labelled "abandoned".
const ABANDONED_DAYS: i64 = 730;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OutdatedSeverity {
    Major,
    Minor,
    Patch,
}

impl OutdatedSeverity {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Patch => "patch",
            Self::Minor => "minor",
            Self::Major => "major",
        }
    }
}

impl Serialize for OutdatedSeverity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum StaleSeverity {
    Abandoned,
    Stale,
}

impl StaleSeverity {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Abandoned => "abandoned",
            Self::Stale => "stale",
        }
    }
}

impl Serialize for StaleSeverity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Serialize)]
pub(super) struct OutdatedEvent {
    #[serde(rename = "crate")]
    pub(super) krate: String,
    /// Version currently resolved in Cargo.lock.
    pub(super) installed: String,
    /// Newest version available on crates.io.
    pub(super) latest: String,
    /// `ccu`'s severity classification: `patch`, `minor`, or `major`.
    pub(super) severity: OutdatedSeverity,
    /// Manifest path where this dep is declared (relative to project
    /// root, as `ccu` reports it).
    pub(super) source_file: String,
    /// Line number in that manifest, useful for jump-to.
    pub(super) line_number: u64,
}

#[derive(Serialize)]
pub(super) struct StaleEvent {
    #[serde(rename = "crate")]
    pub(super) krate: String,
    /// Version on the registry whose release date is the basis for
    /// this event (i.e. the latest available, not the installed one).
    pub(super) version: String,
    /// ISO-8601 timestamp, verbatim from ccu / crates.io.
    pub(super) released_at: String,
    /// Days since `released_at` as of the run.
    pub(super) age_days: u64,
    /// Severity follows `STALE_DAYS` and `ABANDONED_DAYS` below.
    pub(super) severity: StaleSeverity,
}

/// Emitted when `ccu` cannot produce results for either network phase.
/// `phase` remains the original primary label; `phases` is the additive
/// list of every phase covered by this tool invocation.
#[derive(Serialize)]
pub(super) struct ToolMissingEvent {
    pub(super) phase: Phase,
    pub(super) tool: &'static str,
    pub(super) reason: String,
    pub(super) phases: Vec<Phase>,
}

#[derive(Deserialize)]
struct CcuOutput {
    schema_version: u32,
    #[serde(default)]
    checks: Vec<CcuCheck>,
}

#[derive(Deserialize)]
struct CcuCheck {
    dependency: CcuDependency,
    /// `null` when the dep is declared in Cargo.toml but not resolved
    /// into Cargo.lock (workspace-only, platform-gated, or behind an
    /// inactive feature). Such entries have nothing to upgrade so the
    /// Outdated phase skips them; Stale still considers them since the
    /// crate may be abandoned regardless of local use.
    installed: Option<String>,
    latest: String,
    /// `null` when up-to-date; otherwise `"patch"` / `"minor"` /
    /// `"major"`.
    severity: Option<OutdatedSeverity>,
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

pub(super) fn run(project_root: &Path) -> Vec<PhaseOutcome> {
    match try_run(project_root) {
        Ok((outdated_events, stale_events)) => PHASES
            .into_iter()
            .zip([outdated_events, stale_events])
            .map(|(phase, events)| PhaseOutcome::ran(phase, events))
            .collect(),
        Err(reason) => PHASES
            .into_iter()
            .map(|phase| {
                let events = if phase == Phase::Outdated {
                    vec![DepsEvent::ToolMissing(ToolMissingEvent {
                        phase: Phase::Outdated,
                        tool: TOOL,
                        reason: reason.clone(),
                        phases: PHASES.to_vec(),
                    })]
                } else {
                    Vec::new()
                };
                PhaseOutcome::skipped(phase, reason.clone(), events)
            })
            .collect(),
    }
}

fn try_run(project_root: &Path) -> Result<(Vec<DepsEvent>, Vec<DepsEvent>), String> {
    let run = match output::run_captured_with_env_and_deadline(
        TOOL,
        &["--json"],
        project_root,
        &[],
        CCU_DEADLINE,
        None,
        crate::shutdown::ChildIsolation::Inherit,
    ) {
        Ok(r) => r,
        Err(DevError::Spawn { error, .. }) if error.kind() == ErrorKind::NotFound => {
            return Err(INSTALL_HINT.to_string());
        }
        Err(err) => return Err(format!("failed to run {TOOL}: {err}")),
    };
    if run.killed_on_deadline() {
        return Err(format!(
            "killed after {}s with no answer (network trouble?)",
            CCU_DEADLINE.as_secs()
        ));
    }
    let output = run.captured;
    if !output.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output
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
    let parsed: CcuOutput =
        serde_json::from_str(&stdout).map_err(|e| format!("could not parse --json output: {e}"))?;
    if parsed.schema_version != SUPPORTED_SCHEMA {
        return Err(format!(
            "schema_version={} but brokkr expects {SUPPORTED_SCHEMA}",
            parsed.schema_version
        ));
    }

    let now_jdn = now_julian_day(SystemTime::now())?;
    Ok(events_from_output(parsed, now_jdn))
}

fn events_from_output(parsed: CcuOutput, now_jdn: i64) -> (Vec<DepsEvent>, Vec<DepsEvent>) {
    let mut outdated_events = Vec::new();
    let mut stale_events = Vec::new();
    // In multi-workspace projects ccu reports the same crate once per
    // workspace member that depends on it. The findings are identical
    // (same Cargo.lock, same registry data) - we only want to surface
    // each unique smell once.
    let mut seen_outdated: HashSet<(String, String, String)> = HashSet::new();
    let mut seen_stale: HashSet<(String, String)> = HashSet::new();
    for check in parsed.checks {
        if let Some(severity) = check.severity
            && let Some(installed) = check.installed.clone()
        {
            let key = (
                check.dependency.name.clone(),
                installed.clone(),
                check.latest.clone(),
            );
            if seen_outdated.insert(key) {
                outdated_events.push(DepsEvent::Outdated(OutdatedEvent {
                    krate: check.dependency.name.clone(),
                    installed,
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
            stale_events.push(DepsEvent::Stale(StaleEvent {
                krate: check.dependency.name,
                version: check.latest,
                released_at: released_at.clone(),
                age_days: u64::try_from(age_days).unwrap_or(0),
                severity,
            }));
        }
    }
    // Retain the marker at the end for existing NDJSON consumers. The
    // summary now carries each successful phase's zero-result state too.
    stale_events.push(DepsEvent::OutdatedComplete);
    (outdated_events, stale_events)
}

fn classify_age(age_days: i64) -> Option<StaleSeverity> {
    if age_days >= ABANDONED_DAYS {
        Some(StaleSeverity::Abandoned)
    } else if age_days >= STALE_DAYS {
        Some(StaleSeverity::Stale)
    } else {
        None
    }
}

/// Returns days between `now` and the date encoded in `released_at`.
/// Accepts any ISO-8601-like string whose first 10 characters are
/// `YYYY-MM-DD`; the rest (time, fraction, timezone offset) is
/// ignored - day granularity is plenty for the 240-day age threshold.
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

fn now_julian_day(now: SystemTime) -> Result<i64, String> {
    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch".to_string())?
        .as_secs();
    // 2440588 is the JDN of 1970-01-01. as_secs() returns u64;
    // dividing by 86400 brings it well within i64 range for any
    // sane wall-clock value.
    Ok(2440588 + i64::try_from(now_secs / 86400).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
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
        assert_eq!(classify_age(240), Some(StaleSeverity::Stale));
        assert_eq!(classify_age(729), Some(StaleSeverity::Stale));
        assert_eq!(classify_age(730), Some(StaleSeverity::Abandoned));
        assert_eq!(classify_age(3000), Some(StaleSeverity::Abandoned));
    }

    #[test]
    fn julian_day_uses_injected_time_and_rejects_pre_epoch() {
        let epoch = UNIX_EPOCH;
        assert_eq!(now_julian_day(epoch).unwrap(), 2440588);
        assert_eq!(
            now_julian_day(epoch + Duration::from_secs(86400)).unwrap(),
            2440589
        );
        assert!(now_julian_day(epoch - Duration::from_secs(1)).is_err());
    }

    #[test]
    fn output_conversion_deduplicates_findings_and_keeps_the_completion_marker() {
        let check = || CcuCheck {
            dependency: CcuDependency {
                name: "sample".into(),
                source_file: "Cargo.toml".into(),
                line_number: 7,
            },
            installed: Some("1.0.0".into()),
            latest: "2.0.0".into(),
            severity: Some(OutdatedSeverity::Major),
            latest_released_at: Some("2020-01-01T00:00:00Z".into()),
        };
        let parsed = CcuOutput {
            schema_version: SUPPORTED_SCHEMA,
            checks: vec![check(), check()],
        };
        let (outdated, stale) = events_from_output(parsed, julian_day(2023, 1, 1));
        assert_eq!(outdated.len(), 1);
        assert_eq!(stale.len(), 2);
        assert!(matches!(stale.last(), Some(DepsEvent::OutdatedComplete)));
    }
}
