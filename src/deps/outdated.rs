//! `outdated` phase (network).
//!
//! Shells out to `ccu --json` (the user's check-updates tool, replacing
//! `cargo-outdated`). `ccu` queries crates.io and reports every direct
//! dep with a `severity` of `patch` / `minor` / `major`, or `null` if
//! up-to-date. We forward only the non-null entries as `OutdatedEvent`s.
//!
//! Schema version pinned at 1 - the JSON contract is in
//! `~/Programs/check-updates/ccu`.
//!
//! All failure modes (tool missing, subprocess error, non-zero exit,
//! schema mismatch, parse error) collapse into a single `ToolMissing`
//! event. The outdated check is informational - if it can't run, that
//! shouldn't fail the whole `brokkr deps` invocation.

use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;

use super::{DepsEvent, OutdatedEvent, ToolMissingEvent};

const TOOL: &str = "ccu";
const PHASE: &str = "outdated";
const INSTALL_HINT: &str = "not installed; cargo install --path ~/Programs/check-updates/ccu";
const SUPPORTED_SCHEMA: u32 = 1;

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
            phase: PHASE,
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

    let mut events = Vec::new();
    for check in parsed.checks {
        let Some(severity) = check.severity else {
            continue;
        };
        events.push(DepsEvent::Outdated(OutdatedEvent {
            krate: check.dependency.name,
            installed: check.installed,
            latest: check.latest,
            severity,
            source_file: check.dependency.source_file,
            line_number: check.dependency.line_number,
        }));
    }
    Ok(events)
}
