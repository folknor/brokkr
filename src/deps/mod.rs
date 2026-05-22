//! `brokkr deps` - dependency audit.
//!
//! Phase-based: each phase reads `cargo metadata` (parsed once) and emits
//! zero or more `DepsEvent`s. The renderer turns events into prefixed text
//! or NDJSON, depending on `--json`.
//!
//! v1 phases: `duplicate_version`. See `docs/commands/deps.md` for the
//! full design and the planned/idea phases.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::DevError;
use crate::output;

mod duplicate_version;

// --- Event model (NDJSON-serializable) ---

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DepsEvent {
    DuplicateVersion(DuplicateVersionEvent),
    Summary(SummaryEvent),
}

#[derive(Serialize)]
pub struct DuplicateVersionEvent {
    #[serde(rename = "crate")]
    pub krate: String,
    pub pins: Vec<VersionPin>,
}

#[derive(Serialize)]
pub struct VersionPin {
    pub version: String,
    /// Names of workspace direct deps (or workspace members themselves)
    /// that anchor this version. The user yells at these to upgrade.
    pub direct_blame: Vec<String>,
    /// Distinct paths from a workspace member to this (crate, version).
    /// Each entry is a chain of "name version" labels, root first.
    pub paths: Vec<Vec<String>>,
}

#[derive(Serialize)]
pub struct SummaryEvent {
    pub phases_run: Vec<&'static str>,
    pub findings: usize,
}

// --- Cargo metadata (minimal subset we use) ---

#[derive(Deserialize)]
pub(crate) struct CargoMetadata {
    pub packages: Vec<CargoPackage>,
    pub workspace_members: Vec<String>,
    pub resolve: CargoResolve,
}

#[derive(Deserialize)]
pub(crate) struct CargoPackage {
    pub name: String,
    pub version: String,
    pub id: String,
}

#[derive(Deserialize)]
pub(crate) struct CargoResolve {
    pub nodes: Vec<ResolveNode>,
}

#[derive(Deserialize)]
pub(crate) struct ResolveNode {
    pub id: String,
    pub dependencies: Vec<String>,
}

// --- Entry point ---

pub struct DepsArgs {
    pub json: bool,
    pub limit: usize,
    pub all: bool,
    pub no_fail: bool,
}

pub fn run(project_root: &Path, args: &DepsArgs) -> Result<(), DevError> {
    let metadata = load_metadata(project_root)?;
    let mut events = Vec::new();
    let phases_run = vec!["duplicate_version"];

    let dup_events = duplicate_version::run(&metadata);
    let findings = dup_events.len();
    events.extend(dup_events.into_iter().map(DepsEvent::DuplicateVersion));

    events.push(DepsEvent::Summary(SummaryEvent {
        phases_run,
        findings,
    }));

    if args.json {
        render_json(&events)?;
    } else {
        render_text(&events, args.limit, args.all);
    }

    if findings > 0 && !args.no_fail {
        return Err(DevError::ExitCode(1));
    }
    Ok(())
}

fn load_metadata(project_root: &Path) -> Result<CargoMetadata, DevError> {
    let captured = output::run_captured(
        "cargo",
        &["metadata", "--format-version", "1"],
        project_root,
    )?;
    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!("cargo metadata failed: {stderr}")));
    }
    let stdout = String::from_utf8_lossy(&captured.stdout);
    let metadata: CargoMetadata = serde_json::from_str(&stdout)?;
    Ok(metadata)
}

// --- Rendering ---

fn render_json(events: &[DepsEvent]) -> Result<(), DevError> {
    for event in events {
        let line = serde_json::to_string(event)?;
        println!("{line}");
    }
    Ok(())
}

fn render_text(events: &[DepsEvent], limit: usize, all: bool) {
    let dups: Vec<&DuplicateVersionEvent> = events
        .iter()
        .filter_map(|e| match e {
            DepsEvent::DuplicateVersion(d) => Some(d),
            _ => None,
        })
        .collect();

    if dups.is_empty() {
        output::deps_msg("no duplicate versions");
    } else {
        let shown = if all { dups.len() } else { limit.min(dups.len()) };
        output::deps_msg(&format!(
            "{} crate{} with multiple versions:",
            dups.len(),
            if dups.len() == 1 { "" } else { "s" }
        ));
        for dup in dups.iter().take(shown) {
            render_duplicate_text(dup);
        }
        if shown < dups.len() {
            output::deps_msg(&format!(
                "  ... and {} more (use --all to show)",
                dups.len() - shown
            ));
        }
    }

    if let Some(DepsEvent::Summary(s)) = events.last() {
        output::deps_msg(&format!(
            "ran {} phase{}, {} finding{}",
            s.phases_run.len(),
            if s.phases_run.len() == 1 { "" } else { "s" },
            s.findings,
            if s.findings == 1 { "" } else { "s" },
        ));
    }
}

fn render_duplicate_text(dup: &DuplicateVersionEvent) {
    output::deps_msg(&format!("  {}: {} versions", dup.krate, dup.pins.len()));
    for pin in &dup.pins {
        let blame = if pin.direct_blame.is_empty() {
            "(unknown)".to_string()
        } else {
            pin.direct_blame.join(", ")
        };
        output::deps_msg(&format!("    {}  blamed on: {}", pin.version, blame));
        for path in &pin.paths {
            output::deps_msg(&format!("        {}", path.join(" -> ")));
        }
    }
}
