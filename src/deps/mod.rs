//! `brokkr deps` - dependency audit.
//!
//! Phase-based full audit: `run()` loads full and host-filtered `cargo metadata`
//! snapshots for the phases that need them. Focus mode resolves chains from the
//! host-filtered graph and loads the full graph only when it needs a fallback.
//! The renderer turns events into prefixed text or NDJSON, depending on `--json`.
//!
//! Shipped phases and planned work are listed in `docs/commands/deps.md`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::DevError;
use crate::output;

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

// --- Cargo metadata (minimal subset we use) ---

#[derive(Deserialize)]
pub(crate) struct CargoMetadata {
    pub packages: Vec<CargoPackage>,
    pub workspace_members: Vec<String>,
    /// Null under `cargo metadata --no-deps`, which `publish_cycle`'s
    /// standalone entry point uses - it reads declared manifests only, so
    /// paying for the resolve graph would buy nothing. Defaults to empty
    /// rather than erroring so that loader stays a one-liner.
    #[serde(default, deserialize_with = "null_as_default")]
    pub resolve: CargoResolve,
    /// Cargo's absolute workspace root directory (holds the root
    /// `Cargo.toml` with `[workspace.dependencies]`). Required by the metadata
    /// format and used by `workspace_dep`.
    pub workspace_root: String,
}

#[derive(Deserialize)]
pub(crate) struct CargoPackage {
    pub name: String,
    pub version: String,
    pub id: String,
    #[serde(default)]
    pub source: Option<String>,
    pub manifest_path: String,
    /// The manifest's `links` key, if set - the canonical marker for a
    /// crate that links a native library. Used by `native_code`.
    #[serde(default)]
    pub links: Option<String>,
    /// The manifest's `publish` key. `Some([])` is `publish = false`;
    /// `None` is the unrestricted default. Used by `publish_cycle`.
    #[serde(default)]
    pub publish: Option<Vec<String>>,
    /// Declared dependencies from this package's own manifest - what
    /// `cargo publish` writes out, as opposed to `resolve`'s picked
    /// graph. Used by `publish_cycle`.
    #[serde(default)]
    pub dependencies: Vec<PackageDep>,
}

/// One entry of `packages[].dependencies`.
#[derive(Deserialize)]
pub(crate) struct PackageDep {
    pub name: String,
    /// `None` is Normal; `Some("dev")` / `Some("build")` are the others.
    #[serde(default)]
    pub kind: Option<String>,
    /// The version requirement. Cargo renders "none specified" - the
    /// path-only form that publish strips from a dev-dep - as `*`.
    #[serde(default)]
    pub req: String,
}

#[derive(Deserialize, Default)]
pub(crate) struct CargoResolve {
    pub nodes: Vec<ResolveNode>,
}

/// `serde` treats an explicit `null` as a hard error even on a `default`
/// field; cargo writes `"resolve": null` under `--no-deps`, so map it.
fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Deserialize)]
pub(crate) struct ResolveNode {
    pub id: String,
    /// Per-dep structured info. `dep_kinds[*].kind == null` is the Normal
    /// kind we care about; `"dev"` and `"build"` get dropped before blame.
    /// Cargo always populates this; default keeps deserialization
    /// resilient if a future schema change drops it. We deliberately do
    /// not deserialize the sibling `dependencies` array (the flat union
    /// across all targets/features/kinds) - `deps` carries the same set
    /// plus the kind metadata we need to filter on.
    #[serde(default)]
    pub deps: Vec<NodeDep>,
}

#[derive(Deserialize)]
pub(crate) struct NodeDep {
    pub pkg: String,
    #[serde(default)]
    pub dep_kinds: Vec<DepKind>,
}

#[derive(Deserialize)]
pub(crate) struct DepKind {
    /// `None` is Normal; `Some("dev")` / `Some("build")` are the others.
    #[serde(default)]
    pub kind: Option<String>,
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

fn load_metadata(project_root: &Path) -> Result<CargoMetadata, DevError> {
    run_metadata_locked(project_root, &["metadata", "--format-version", "1"])
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
fn run_metadata_locked(project_root: &Path, args: &[&str]) -> Result<CargoMetadata, DevError> {
    let mut locked: Vec<&str> = args.to_vec();
    locked.push("--locked");
    run_metadata(project_root, &locked).map_err(|e| match e {
        DevError::Build(msg) => DevError::Build(format!(
            "{msg}\n`brokkr deps` runs cargo metadata with --locked: it audits Cargo.lock as \
             committed and never rewrites it. A missing or out-of-date lockfile must be \
             brought current by a build first."
        )),
        other => other,
    })
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
    let metadata = run_metadata(
        project_root,
        &["metadata", "--format-version", "1", "--no-deps"],
    )?;
    Ok(publish_cycle::run(&metadata))
}

/// Same as `load_metadata` but adds `--filter-platform=<host>` so the
/// resolve graph drops packages and edges that aren't built for this
/// host. Used by `duplicate_version` to avoid blaming the workspace
/// for transitive crates that only exist on other targets (the
/// `cargo tree -i <crate> --target all` case where nothing actually
/// depends on the crate on any active target). Host triple comes from
/// `rustc -vV` field `host:`.
fn load_metadata_host_filtered(project_root: &Path) -> Result<CargoMetadata, DevError> {
    let host = host_triple()?;
    run_metadata_locked(
        project_root,
        &[
            "metadata",
            "--format-version",
            "1",
            "--filter-platform",
            &host,
        ],
    )
}

fn run_metadata(project_root: &Path, args: &[&str]) -> Result<CargoMetadata, DevError> {
    let captured = output::run_captured("cargo", args, project_root)?;
    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!("cargo metadata failed: {stderr}")));
    }
    let stdout = String::from_utf8_lossy(&captured.stdout);
    let metadata: CargoMetadata = serde_json::from_str(&stdout)?;
    Ok(metadata)
}

fn host_triple() -> Result<String, DevError> {
    let captured = output::run_captured("rustc", &["-vV"], Path::new("."))?;
    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!("rustc -vV failed: {stderr}")));
    }
    let stdout = String::from_utf8_lossy(&captured.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("host: ") {
            return Ok(rest.trim().to_string());
        }
    }
    Err(DevError::Build(
        "rustc -vV did not report a host triple".to_string(),
    ))
}
