use super::DepsEvent;
use super::ccu::{OutdatedEvent, StaleEvent};
use super::duplicate_version::DuplicateVersionEvent;
use super::git_dependency::{GitDependencyEvent, GitRef};
use super::native_code::NativeDependencyEvent;
use super::path_dependency::PathDependencyEvent;
use super::phase::{Phase, PhaseStatus};
use super::publish_cycle::{self, PublishCycleEvent};
use super::workspace_dep::UnusedWorkspaceDepEvent;
use crate::error::DevError;
use crate::output;

trait Render {
    fn render(&self);
}

pub(super) fn render_json(events: &[DepsEvent]) -> Result<(), DevError> {
    for event in events {
        let line = serde_json::to_string(event)?;
        println!("{line}");
    }
    Ok(())
}

pub(super) fn render_text(events: &[DepsEvent]) {
    let mut dups = Vec::new();
    let mut gits = Vec::new();
    let mut paths = Vec::new();
    let mut cycles = Vec::new();
    let mut ws = Vec::new();
    let mut native = Vec::new();
    let mut outdated = Vec::new();
    let outdated_ran = events
        .iter()
        .find_map(|event| match event {
            DepsEvent::Summary(summary) => summary
                .phase_outcomes
                .iter()
                .find(|outcome| outcome.phase == Phase::Outdated)
                .map(|outcome| matches!(&outcome.status, PhaseStatus::Ran)),
            _ => None,
        })
        .unwrap_or(false);
    let mut stale = Vec::new();
    let mut missing = Vec::new();
    for e in events {
        match e {
            DepsEvent::DuplicateVersion(d) => dups.push(d),
            DepsEvent::GitDependency(g) => gits.push(g),
            DepsEvent::PathDependency(p) => paths.push(p),
            DepsEvent::PublishCycle(c) => cycles.push(c),
            DepsEvent::UnusedWorkspaceDep(w) => ws.push(w),
            DepsEvent::NativeDependency(n) => native.push(n),
            DepsEvent::Outdated(o) => outdated.push(o),
            DepsEvent::OutdatedComplete => {}
            DepsEvent::Stale(s) => stale.push(s),
            DepsEvent::ToolMissing(t) => missing.push(t),
            DepsEvent::Summary(_) => {}
        }
    }
    outdated.sort_by(|a, b| a.severity.cmp(&b.severity).then(a.krate.cmp(&b.krate)));
    // Abandoned first, then stale; within each, oldest first.
    stale.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then(b.age_days.cmp(&a.age_days))
    });

    render_dup_section(&dups);
    render_section(&gits, "git dependency", "git dependencies", "");
    render_section(
        &paths,
        "path dependency",
        "path dependencies",
        "outside workspace",
    );
    render_section(
        &cycles,
        "cargo publication cycle",
        "cargo publication cycles",
        "",
    );
    if !cycles.is_empty() {
        output::deps_msg(&format!("  {}", publish_cycle::INVENTORY_CAVEAT));
    }
    render_section(
        &ws,
        "unused workspace dependency",
        "unused workspace dependencies",
        "not inherited by any member",
    );
    render_section(
        &native,
        "dependency with native code",
        "dependencies with native code",
        "",
    );
    render_outdated_section(&outdated, outdated_ran);
    render_section(&stale, "stale dependency", "stale dependencies", "");

    for tool_missing in &missing {
        let phases = tool_missing
            .phases
            .iter()
            .map(|phase| phase.as_str())
            .collect::<Vec<_>>()
            .join(" and ");
        output::deps_msg(&format!(
            "{} skipped ({}): {}",
            phases, tool_missing.tool, tool_missing.reason,
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

fn render_dup_section(items: &[&DuplicateVersionEvent]) {
    if items.is_empty() {
        return;
    }
    let noun = if items.len() == 1 { "crate" } else { "crates" };
    output::deps_msg(&format!("{} {noun} with multiple versions:", items.len()));
    for item in items {
        item.render();
    }
}

/// The outdated section communicates an exhaustive answer, not a
/// partial finding: if a direct dep isn't here, it has no newer
/// version on crates.io. Header phrasing and the explicit zero-case
/// line both lean on that. When ccu didn't run at all, this prints
/// nothing - the accompanying `ToolMissing` event covers it.
fn render_outdated_section(items: &[&OutdatedEvent], outdated_ran: bool) {
    if !outdated_ran {
        return;
    }
    if items.is_empty() {
        output::deps_msg("All direct deps are at latest on crates.io.");
        return;
    }
    output::deps_msg(&format!(
        "{} available on crates.io; no other candidates:",
        output::count(items.len(), "upgrade")
    ));
    for item in items {
        item.render();
    }
}

fn render_section<T: Render>(items: &[&T], singular: &str, plural: &str, suffix: &str) {
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
    for item in items {
        item.render();
    }
}

fn render_duplicate_text(dup: &DuplicateVersionEvent) {
    output::deps_msg(&format!("  {}: {} versions", dup.krate, dup.pins.len()));
    for pin in &dup.pins {
        let picked = if pin.picked_by.is_empty() {
            "(unknown)".to_string()
        } else {
            pin.picked_by.join(", ")
        };
        let suffix = if pin.via_workspace.is_empty() {
            String::new()
        } else {
            format!("  [via {}]", pin.via_workspace.join(", "))
        };
        output::deps_msg(&format!("    {}  picked by {picked}{suffix}", pin.version));
    }
}

fn render_git_text(git: &GitDependencyEvent) {
    let ref_part = match &git.requested_ref {
        GitRef::Branch(branch) => format!("branch={branch}"),
        GitRef::Tag(tag) => format!("tag={tag}"),
        GitRef::Rev(rev) => format!("rev={rev}"),
        GitRef::DefaultBranch => "default branch".to_string(),
    };
    let resolved = git
        .resolved_rev
        .as_deref()
        .map(|rev| format!(" # {rev}"))
        .unwrap_or_default();
    output::deps_msg(&format!(
        "  {} {}  {} @ {}{}",
        git.krate, git.version, git.url, ref_part, resolved
    ));
}

fn render_path_text(path: &PathDependencyEvent) {
    output::deps_msg(&format!(
        "  {} {}  {}",
        path.krate, path.version, path.manifest_path
    ));
}

/// Prints the loop as a chain, then - when the cycle is closed by a
/// dev-dependency that names a version - the one-line fix, since that
/// edge is removable without restructuring anything. The lines themselves
/// come from `publish_cycle`, shared with the `brokkr check` phase so the
/// two commands describe an identical cycle identically.
fn render_cycle_text(c: &PublishCycleEvent) {
    for line in publish_cycle::render_lines(c) {
        output::deps_msg(&format!("  {line}"));
    }
}

fn render_ws_text(w: &UnusedWorkspaceDepEvent) {
    output::deps_msg(&format!("  {}", w.krate));
}

fn render_native_text(n: &NativeDependencyEvent) {
    let mut parts = Vec::new();
    if !n.toolchains.is_empty() {
        parts.push(format!("compiles ({})", n.toolchains.join(", ")));
    }
    if let Some(lib) = &n.links {
        parts.push(format!("links {lib}"));
    }
    output::deps_msg(&format!(
        "  {} {}  {}",
        n.krate,
        n.version,
        parts.join(" + ")
    ));
}

fn render_outdated_text(o: &OutdatedEvent) {
    output::deps_msg(&format!(
        "  {}: {} {} -> {}  ({}:{})",
        o.severity.as_str(),
        o.krate,
        o.installed,
        o.latest,
        o.source_file,
        o.line_number,
    ));
}

fn render_stale_text(s: &StaleEvent) {
    output::deps_msg(&format!(
        "  {}: {} (last release {} ago)",
        s.severity.as_str(),
        s.krate,
        human_age(s.age_days),
    ));
}

impl Render for DuplicateVersionEvent {
    fn render(&self) {
        render_duplicate_text(self);
    }
}

impl Render for GitDependencyEvent {
    fn render(&self) {
        render_git_text(self);
    }
}

impl Render for PathDependencyEvent {
    fn render(&self) {
        render_path_text(self);
    }
}

impl Render for PublishCycleEvent {
    fn render(&self) {
        render_cycle_text(self);
    }
}

impl Render for UnusedWorkspaceDepEvent {
    fn render(&self) {
        render_ws_text(self);
    }
}

impl Render for NativeDependencyEvent {
    fn render(&self) {
        render_native_text(self);
    }
}

impl Render for OutdatedEvent {
    fn render(&self) {
        render_outdated_text(self);
    }
}

impl Render for StaleEvent {
    fn render(&self) {
        render_stale_text(self);
    }
}

/// Coarse years-and-months age. Months are 30-day buckets of the remainder
/// under 365, so days 360-364 would divide out to 12 - clamped to 11, since
/// "12mo" (or "1y12mo") is a year the year column already owns.
fn human_age(days: u64) -> String {
    let years = days / 365;
    let months = ((days % 365) / 30).min(11);
    match (years, months) {
        (0, m) => format!("{}mo", m.max(1)),
        (y, 0) => format!("{y}y"),
        (y, m) => format!("{y}y{m}mo"),
    }
}

#[cfg(test)]
mod tests {
    use super::human_age;

    #[test]
    fn human_age_never_prints_twelve_months() {
        assert_eq!(human_age(0), "1mo");
        assert_eq!(human_age(45), "1mo");
        assert_eq!(human_age(364), "11mo");
        assert_eq!(human_age(365), "1y");
        assert_eq!(human_age(365 + 362), "1y11mo");
        assert_eq!(human_age(800), "2y2mo");
    }
}
