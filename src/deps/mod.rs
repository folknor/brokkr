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
mod git_dependency;
mod path_dependency;

// --- Event model (NDJSON-serializable) ---

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DepsEvent {
    DuplicateVersion(DuplicateVersionEvent),
    GitDependency(GitDependencyEvent),
    PathDependency(PathDependencyEvent),
    Summary(SummaryEvent),
}

#[derive(Serialize)]
pub struct GitDependencyEvent {
    #[serde(rename = "crate")]
    pub krate: String,
    pub version: String,
    /// Repo URL with the `git+` prefix stripped.
    pub url: String,
    /// Resolved commit SHA (from the source URL fragment, the lockfile's
    /// pinned commit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// Branch name if the manifest requested a branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Tag name if the manifest requested a tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

#[derive(Serialize)]
pub struct PathDependencyEvent {
    #[serde(rename = "crate")]
    pub krate: String,
    pub version: String,
    /// Absolute path to the dep's `Cargo.toml`.
    pub manifest_path: String,
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
    #[serde(default)]
    pub source: Option<String>,
    pub manifest_path: String,
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
    pub chains: bool,
    pub no_fail: bool,
}

pub fn run(project_root: &Path, args: &DepsArgs) -> Result<(), DevError> {
    let metadata = load_metadata(project_root)?;
    let mut events = Vec::new();
    let phases_run = vec!["duplicate_version", "git_dependency", "path_dependency"];

    let dup_events = duplicate_version::run(&metadata);
    let git_events = git_dependency::run(&metadata);
    let path_events = path_dependency::run(&metadata);

    let findings = dup_events.len() + git_events.len() + path_events.len();

    events.extend(dup_events.into_iter().map(DepsEvent::DuplicateVersion));
    events.extend(git_events.into_iter().map(DepsEvent::GitDependency));
    events.extend(path_events.into_iter().map(DepsEvent::PathDependency));

    events.push(DepsEvent::Summary(SummaryEvent {
        phases_run,
        findings,
    }));

    if args.json {
        render_json(&events)?;
    } else {
        // --all implies --chains; chains is just the per-pin chain
        // toggle, --all also uncaps the per-phase item count.
        let show_chains = args.chains || args.all;
        render_text(&events, args.limit, args.all, show_chains);
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

fn render_text(events: &[DepsEvent], limit: usize, all: bool, show_chains: bool) {
    let mut dups = Vec::new();
    let mut gits = Vec::new();
    let mut paths = Vec::new();
    for e in events {
        match e {
            DepsEvent::DuplicateVersion(d) => dups.push(d),
            DepsEvent::GitDependency(g) => gits.push(g),
            DepsEvent::PathDependency(p) => paths.push(p),
            DepsEvent::Summary(_) => {}
        }
    }

    render_dup_section(&dups, limit, all, show_chains);
    render_section(&gits, "git dependency", "git dependencies", "", limit, all, render_git_text);
    render_section(&paths, "path dependency", "path dependencies", "outside workspace", limit, all, render_path_text);

    if let Some(DepsEvent::Summary(s)) = events.last() {
        if s.findings == 0 {
            output::deps_msg(&format!("ran {} phases, no findings", s.phases_run.len()));
        } else {
            output::deps_msg(&format!(
                "ran {} phases, {} finding{}",
                s.phases_run.len(),
                s.findings,
                if s.findings == 1 { "" } else { "s" },
            ));
        }
    }
}

/// When chains are shown and `--all` is not set, cap the number of
/// dep-chain examples per duplicated `(crate, version)`. The blame
/// anchor list above each chain already captures the actionable
/// signal; the chains are tracing detail. In big dep trees the same
/// chain repeats per workspace member, which drowns the report.
const PATHS_PER_PIN: usize = 3;

fn render_dup_section(
    items: &[&DuplicateVersionEvent],
    limit: usize,
    all: bool,
    show_chains: bool,
) {
    if items.is_empty() {
        return;
    }
    let noun = if items.len() == 1 { "crate" } else { "crates" };
    output::deps_msg(&format!(
        "{} {noun} with multiple versions:",
        items.len()
    ));
    let shown = if all { items.len() } else { limit.min(items.len()) };
    for item in items.iter().take(shown) {
        render_duplicate_text(item, show_chains, all);
    }
    if shown < items.len() {
        output::deps_msg(&format!(
            "  ... and {} more (use --all to show)",
            items.len() - shown
        ));
    }
}

fn render_section<T>(
    items: &[&T],
    singular: &str,
    plural: &str,
    suffix: &str,
    limit: usize,
    all: bool,
    render_one: fn(&T),
) {
    if items.is_empty() {
        return;
    }
    let noun = if items.len() == 1 { singular } else { plural };
    let header = if suffix.is_empty() {
        format!("{} {noun}:", items.len())
    } else {
        format!("{} {noun} {suffix}:", items.len())
    };
    output::deps_msg(&header);
    let shown = if all { items.len() } else { limit.min(items.len()) };
    for item in items.iter().take(shown) {
        render_one(item);
    }
    if shown < items.len() {
        output::deps_msg(&format!(
            "  ... and {} more (use --all to show)",
            items.len() - shown
        ));
    }
}

fn render_duplicate_text(dup: &DuplicateVersionEvent, show_chains: bool, all: bool) {
    output::deps_msg(&format!("  {}: {} versions", dup.krate, dup.pins.len()));
    for pin in &dup.pins {
        let blame = if pin.direct_blame.is_empty() {
            "(unknown)".to_string()
        } else {
            pin.direct_blame.join(", ")
        };
        output::deps_msg(&format!("    {}  blamed on: {}", pin.version, blame));
        if !show_chains {
            continue;
        }
        let path_cap = if all {
            pin.paths.len()
        } else {
            PATHS_PER_PIN.min(pin.paths.len())
        };
        for path in pin.paths.iter().take(path_cap) {
            output::deps_msg(&format!("        {}", path.join(" -> ")));
        }
        if path_cap < pin.paths.len() {
            output::deps_msg(&format!(
                "        ... and {} more chain{} (use --all)",
                pin.paths.len() - path_cap,
                if pin.paths.len() - path_cap == 1 { "" } else { "s" },
            ));
        }
    }
}

fn render_git_text(git: &GitDependencyEvent) {
    let ref_part = match (&git.tag, &git.branch, &git.rev) {
        (Some(t), _, _) => format!("tag={t}"),
        (_, Some(b), _) => format!("branch={b}"),
        (_, _, Some(r)) => format!("rev={r}"),
        _ => "(default branch)".to_string(),
    };
    output::deps_msg(&format!(
        "  {} {}  {} @ {}",
        git.krate, git.version, git.url, ref_part
    ));
}

fn render_path_text(path: &PathDependencyEvent) {
    output::deps_msg(&format!(
        "  {} {}  {}",
        path.krate, path.version, path.manifest_path
    ));
}
