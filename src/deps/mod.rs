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

mod ccu;
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
    Outdated(OutdatedEvent),
    Stale(StaleEvent),
    ToolMissing(ToolMissingEvent),
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
pub struct OutdatedEvent {
    #[serde(rename = "crate")]
    pub krate: String,
    /// Version currently resolved in Cargo.lock.
    pub installed: String,
    /// Newest version available on crates.io.
    pub latest: String,
    /// `ccu`'s severity classification: `patch`, `minor`, or `major`.
    pub severity: String,
    /// Manifest path where this dep is declared (relative to project
    /// root, as `ccu` reports it).
    pub source_file: String,
    /// Line number in that manifest, useful for jump-to.
    pub line_number: u64,
}

#[derive(Serialize)]
pub struct StaleEvent {
    #[serde(rename = "crate")]
    pub krate: String,
    /// Version on the registry whose release date is the basis for
    /// this event (i.e. the latest available, not the installed one).
    pub version: String,
    /// ISO-8601 timestamp, verbatim from ccu / crates.io.
    pub released_at: String,
    /// Days since `released_at` as of the run.
    pub age_days: u64,
    /// `"stale"` (>= ~8 months) or `"abandoned"` (>= ~2 years).
    pub severity: &'static str,
}

/// Emitted when a phase couldn't produce its findings because an
/// external tool was missing or failed to run. Doesn't count as a
/// finding for exit-code purposes - it's a heads-up about the
/// phase, not a smell in the user's code.
#[derive(Serialize)]
pub struct ToolMissingEvent {
    /// Phase that wanted the tool.
    pub phase: &'static str,
    /// Executable name we tried to invoke.
    pub tool: &'static str,
    /// Why the phase was skipped: install hint when the tool isn't on
    /// PATH, error description when it ran but failed.
    pub reason: String,
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
    let phases_run = vec![
        "duplicate_version",
        "git_dependency",
        "path_dependency",
        "outdated",
        "stale",
    ];

    let dup_events = duplicate_version::run(&metadata);
    let git_events = git_dependency::run(&metadata);
    let path_events = path_dependency::run(&metadata);

    // Only offline phases contribute to the failure-counting findings.
    // `outdated` (and future network phases) are informational - a
    // patch bump on a dependency shouldn't fail your build.
    let findings = dup_events.len() + git_events.len() + path_events.len();

    events.extend(dup_events.into_iter().map(DepsEvent::DuplicateVersion));
    events.extend(git_events.into_iter().map(DepsEvent::GitDependency));
    events.extend(path_events.into_iter().map(DepsEvent::PathDependency));
    events.extend(ccu::run(project_root));

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
    let mut outdated = Vec::new();
    let mut stale = Vec::new();
    let mut missing = Vec::new();
    for e in events {
        match e {
            DepsEvent::DuplicateVersion(d) => dups.push(d),
            DepsEvent::GitDependency(g) => gits.push(g),
            DepsEvent::PathDependency(p) => paths.push(p),
            DepsEvent::Outdated(o) => outdated.push(o),
            DepsEvent::Stale(s) => stale.push(s),
            DepsEvent::ToolMissing(t) => missing.push(t),
            DepsEvent::Summary(_) => {}
        }
    }
    outdated.sort_by(|a, b| {
        severity_rank(&a.severity)
            .cmp(&severity_rank(&b.severity))
            .then(a.krate.cmp(&b.krate))
    });
    // Abandoned first, then stale; within each, oldest first.
    stale.sort_by(|a, b| {
        stale_rank(a.severity)
            .cmp(&stale_rank(b.severity))
            .then(b.age_days.cmp(&a.age_days))
    });

    render_dup_section(&dups, limit, all, show_chains);
    render_section(&gits, "git dependency", "git dependencies", "", limit, all, render_git_text);
    render_section(&paths, "path dependency", "path dependencies", "outside workspace", limit, all, render_path_text);
    render_section(&outdated, "outdated dependency", "outdated dependencies", "", limit, all, render_outdated_text);
    render_section(&stale, "stale dependency", "stale dependencies", "", limit, all, render_stale_text);

    for tool_missing in &missing {
        output::deps_msg(&format!(
            "{} skipped ({}): {}",
            tool_missing.phase, tool_missing.tool, tool_missing.reason,
        ));
    }

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

fn render_outdated_text(o: &OutdatedEvent) {
    output::deps_msg(&format!(
        "  {}: {} {} -> {}  ({}:{})",
        o.severity, o.krate, o.installed, o.latest, o.source_file, o.line_number,
    ));
}

fn render_stale_text(s: &StaleEvent) {
    output::deps_msg(&format!(
        "  {}: {} {}  latest released {} ({} ago)",
        s.severity,
        s.krate,
        s.version,
        s.released_at_date(),
        human_age(s.age_days),
    ));
}

/// Lower is more severe. Used for sorting so majors print first.
fn severity_rank(severity: &str) -> u8 {
    match severity {
        "major" => 0,
        "minor" => 1,
        "patch" => 2,
        _ => 3,
    }
}

fn stale_rank(severity: &str) -> u8 {
    match severity {
        "abandoned" => 0,
        "stale" => 1,
        _ => 2,
    }
}

fn human_age(days: u64) -> String {
    let years = days / 365;
    let months = (days % 365) / 30;
    match (years, months) {
        (0, m) => format!("{}mo", m.max(1)),
        (y, 0) => format!("{y}y"),
        (y, m) => format!("{y}y{m}mo"),
    }
}

impl StaleEvent {
    /// First 10 characters of `released_at`, or the whole string if
    /// it's shorter. Lets the text renderer show "2023-04-15" instead
    /// of the full microsecond+timezone form.
    fn released_at_date(&self) -> &str {
        self.released_at.get(..10).unwrap_or(&self.released_at)
    }
}
