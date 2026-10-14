//! `brokkr deps` - dependency audit.
//!
//! Phase-based full audit: `run()` loads one resolving Cargo snapshot and
//! projects its graph to the host for phases that need that view. Focus mode
//! uses the same projection and falls back to the unfiltered graph when needed.
//! The renderer turns events into prefixed text or NDJSON, depending on `--json`.
//!
//! Shipped phases and planned work are listed in `docs/commands/deps.md`.

use std::path::Path;

use serde::Serialize;

use crate::cargo::metadata::{self, Query};
use crate::error::DevError;

mod ccu;
mod duplicate_version;
mod focus;
mod git_dependency;
mod native_code;
mod path_dependency;
mod phase;
mod publish_cycle;
mod render;
mod workspace_dep;

pub use crate::cargo::metadata::DependencyKind;
pub(crate) use crate::cargo::metadata::{
    CargoMetadata, CargoPackage, PackageId, PackageSourceKind,
};
use focus::run_focus;
use phase::{Phase, PhaseOutcome, PhaseSummary};

// --- Event model (NDJSON-serializable) ---

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DepsEvent {
    DuplicateVersion(duplicate_version::DuplicateVersionEvent),
    GitDependency(git_dependency::GitDependencyEvent),
    PathDependency(path_dependency::PathDependencyEvent),
    PublishCycle(publish_cycle::PublishCycleEvent),
    UnusedWorkspaceDep(workspace_dep::UnusedWorkspaceDepEvent),
    NativeDependency(native_code::NativeDependencyEvent),
    Outdated(ccu::OutdatedEvent),
    /// Kept for existing NDJSON consumers. `Summary.phase_outcomes` also
    /// distinguishes a successful zero-result run from a skipped phase.
    OutdatedComplete,
    Stale(ccu::StaleEvent),
    ToolMissing(ccu::ToolMissingEvent),
    Summary(SummaryEvent),
}

#[derive(Serialize)]
struct SummaryEvent {
    phases_run: Vec<Phase>,
    findings: usize,
    phase_outcomes: Vec<PhaseSummary>,
}

// --- Entry point ---

pub struct DepsArgs {
    pub json: bool,
    pub no_fail: bool,
    /// When `Some`, switch into chain-trace mode for the named package
    /// (`"name"` or `"name@version"`). Other phases are suppressed.
    pub focus: Option<String>,
    /// `[deps].workspace_dep_ignore`: workspace deps the unused-workspace-dep
    /// phase must not flag (entries may end in `*` for a prefix match).
    pub workspace_dep_ignore: Vec<String>,
}

pub fn run(project_root: &Path, args: &DepsArgs) -> Result<(), DevError> {
    if let Some(spec) = args.focus.as_deref() {
        return run_focus(project_root, spec, args.json);
    }

    let metadata = load_metadata(project_root)?;
    let host_metadata = load_metadata_host_filtered(project_root)?;
    // duplicate_version uses host-filtered metadata so transitive crates
    // brought in only for inactive targets (e.g. wasmparser -> hashbrown
    // 0.15.5 on a linux host) don't show up as bogus duplicates.
    let mut outcomes = vec![PhaseOutcome::ran(
        Phase::DuplicateVersion,
        duplicate_version::run(&host_metadata)
            .into_iter()
            .map(DepsEvent::DuplicateVersion)
            .collect(),
    )];
    outcomes.push(PhaseOutcome::ran(
        Phase::GitDependency,
        git_dependency::run(&metadata)
            .into_iter()
            .map(DepsEvent::GitDependency)
            .collect(),
    ));
    outcomes.push(PhaseOutcome::ran(
        Phase::PathDependency,
        path_dependency::run(&metadata)
            .into_iter()
            .map(DepsEvent::PathDependency)
            .collect(),
    ));
    // Unfiltered metadata: publication is a property of the declared
    // manifests, so a cycle closed by a target-specific dep still blocks
    // the release even when this host doesn't build that edge.
    outcomes.push(PhaseOutcome::ran(
        Phase::PublishCycle,
        publish_cycle::run(&metadata)
            .into_iter()
            .map(DepsEvent::PublishCycle)
            .collect(),
    ));
    outcomes.push(PhaseOutcome::ran(
        Phase::WorkspaceDep,
        workspace_dep::run(&metadata, &args.workspace_dep_ignore)?
            .into_iter()
            .map(DepsEvent::UnusedWorkspaceDep)
            .collect(),
    ));
    // native_code uses host-filtered metadata like duplicate_version so
    // wasm-only native bundlers (e.g. sqlite-wasm-rs) don't show up on a
    // native host.
    outcomes.push(PhaseOutcome::ran(
        Phase::NativeCode,
        native_code::run(&host_metadata)
            .into_iter()
            .map(DepsEvent::NativeDependency)
            .collect(),
    ));
    outcomes.extend(ccu::run(project_root));

    let findings: usize = outcomes.iter().map(PhaseOutcome::findings).sum();
    let phases_run: Vec<Phase> = outcomes
        .iter()
        .filter(|outcome| outcome.did_run())
        .map(|outcome| outcome.phase)
        .collect();
    let phase_outcomes: Vec<PhaseSummary> = outcomes.iter().map(PhaseOutcome::summary).collect();
    let mut events: Vec<DepsEvent> = outcomes
        .into_iter()
        .flat_map(|outcome| outcome.events)
        .collect();

    events.push(DepsEvent::Summary(SummaryEvent {
        phases_run,
        findings,
        phase_outcomes,
    }));

    if args.json {
        render::render_json(&events)?;
    } else {
        render::render_text(&events);
    }

    if findings > 0 && !args.no_fail {
        return Err(DevError::ExitCode(1));
    }
    Ok(())
}

fn load_metadata(project_root: &Path) -> Result<std::sync::Arc<CargoMetadata>, DevError> {
    metadata::load(
        project_root,
        Query {
            locked: true,
            ..Query::resolving()
        },
        &[],
        None,
    )
    .map_err(with_locked_explanation)
}

/// A resolving `cargo metadata`, pinned with `--locked`. `deps` audits the
/// lockfile as committed; without the flag cargo re-resolves whenever the
/// lock is missing or behind a manifest, rewriting `Cargo.lock` and querying
/// the registry index - so the report would describe a graph that exists
/// nowhere but this run, and the tree would be dirty afterwards. `--offline`
/// is deliberately not passed: metadata reads every dependency's manifest,
/// so an uncached crate source still has to be downloaded, and refusing that
/// would fail on any fresh machine for no correctness gain.
///
/// No lockfile at all is refused rather than generated: there is nothing to
/// audit, and creating one is a build's job, not a report's.
fn with_locked_explanation(error: DevError) -> DevError {
    match error {
        DevError::Build(msg) => DevError::Build(format!(
            "{msg}\n`brokkr deps` runs cargo metadata with --locked: it audits Cargo.lock as \
             committed and never rewrites it. A missing or out-of-date lockfile must be \
             brought current by a build first."
        )),
        other => other,
    }
}

/// The shared renderer: `deps` and the `check` phase print an identical
/// cycle identically.
pub(crate) use publish_cycle::PublishCycleEvent;
pub(crate) use publish_cycle::render_lines as publish_cycle_lines;

/// The shared "this list may be partial" clause, printed by `deps` and `check`.
pub(crate) use publish_cycle::INVENTORY_CAVEAT as PUBLISH_CYCLE_CAVEAT;

/// Run the `publish_cycle` phase on its own, for `brokkr check`.
///
/// The phase reads declared manifests only, so this loads `--no-deps`
/// metadata - the cheap call, no resolve graph, no lockfile update. It is
/// deliberately the *same* `publish_cycle::run` the `deps` command drives:
/// a cycle that fails `brokkr deps` must fail `brokkr check` identically,
/// or `deps` and `check` could disagree about the same tree.
pub(crate) fn publication_cycles(project_root: &Path) -> Result<Vec<PublishCycleEvent>, DevError> {
    let metadata = metadata::load(project_root, Query::workspace(), &[], None)?;
    Ok(publish_cycle::run(&metadata))
}

/// Project the shared resolving graph to the selected rustc host, dropping
/// packages and edges confined to inactive targets. Used by
/// `duplicate_version` to avoid blaming the workspace for transitive crates
/// that only exist on other targets (the `cargo tree -i <crate> --target all`
/// case where nothing actually depends on the crate on an active target).
fn load_metadata_host_filtered(
    project_root: &Path,
) -> Result<std::sync::Arc<CargoMetadata>, DevError> {
    let host = crate::cargo::rustc::load(project_root)?.host.clone();
    let metadata = load_metadata(project_root)?;
    Ok(std::sync::Arc::new(metadata.filtered_for_platform(&host)))
}
