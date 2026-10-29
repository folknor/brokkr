//! `brokkr library`: pbfhogg used as a library, through a harness that a
//! pbfhogg workspace member provides. The protocol between the two sides is
//! `docs/commands/library.md`; this module is brokkr's side of it.
//!
//! - [`json`]: the strict JSON every protocol document is read with.
//! - [`describe`]: the describe document and its validation.
//! - [`run`]: one verification run of a scenario and its verdict.
//! - [`artefact`]: the artefact tree and documents a run leaves.

mod artefact;
mod describe;
mod json;
mod run;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::build::RustProfile;
use crate::config::HarnessConfig;
use crate::datasets::error::ResolveError;
use crate::datasets::resolve::{self, SnapshotRef};
use crate::error::DevError;
use crate::output;
use crate::pbfhogg::binary::LibraryHarness;

/// The config table the harness build comes from, for messages.
const TABLE: &str = "[pbfhogg.library.harness]";

/// What `brokkr library` was asked.
pub(crate) struct LibraryArgs<'a> {
    pub(crate) project_root: &'a Path,
    /// The tree the harness is built from: the checkout, or the `--commit`
    /// worktree.
    pub(crate) build_root: &'a Path,
    pub(crate) harness: &'a HarnessConfig,
    /// `--profile`/`--debug`/`--release`, when given.
    pub(crate) profile: Option<RustProfile>,
    /// A scenario to run; `None` lists them.
    pub(crate) run: Option<RunArgs<'a>>,
}

/// `brokkr library SCENARIO`: what to run, and where its inputs come from.
pub(crate) struct RunArgs<'a> {
    pub(crate) scenario: &'a str,
    pub(crate) dataset: &'a str,
    pub(crate) snapshot: SnapshotRef,
    pub(crate) paths: &'a crate::config::ResolvedPaths,
}

/// `brokkr library [SCENARIO]`: build the harness, read its describe
/// document, then list the scenarios or run one.
pub(crate) fn library(args: &LibraryArgs<'_>) -> Result<(), DevError> {
    let _lock = crate::context::reenter_command_lock()?;
    // One window for the whole command: the build, describe and run guards
    // nest inside it, so a stop between them or during parsing is seen, not
    // taken as the default action past the tree policy's restore.
    let _interrupts = crate::shutdown::SigtermGuard::install();
    let profile = args
        .profile
        .clone()
        .or_else(|| args.harness.configured_profile(TABLE))
        .unwrap_or(RustProfile::Release);
    let harness = LibraryHarness::build(args.harness, profile, args.build_root, args.project_root)?;
    let bytes = harness.describe(describe::DESCRIBE_LIMIT)?;
    let doc = describe::parse(&bytes).map_err(|why| {
        DevError::ContractViolation(format!("the library harness's describe document: {why}"))
    })?;
    if crate::shutdown::should_unwind() {
        return Err(DevError::Interrupted);
    }
    match &args.run {
        None => list(&doc),
        Some(run) => run_one(&harness, &doc, run, args.project_root),
    }
}

fn list(doc: &describe::Describe) -> Result<(), DevError> {
    for line in listing(doc) {
        if crate::shutdown::should_unwind() {
            return Err(DevError::Interrupted);
        }
        output::data_stdout(&line);
    }
    // A stop that landed during the last write is still this command's.
    if crate::shutdown::should_unwind() {
        return Err(DevError::Interrupted);
    }
    Ok(())
}

fn run_one(
    harness: &LibraryHarness,
    doc: &describe::Describe,
    args: &RunArgs<'_>,
    project_root: &Path,
) -> Result<(), DevError> {
    let scenario = find_scenario(doc, args.scenario)?;
    let datasets = bind_datasets(
        scenario,
        |variant| {
            resolve::resolve_snapshot_pbf_path(
                args.dataset,
                &args.snapshot,
                variant,
                args.paths,
                project_root,
            )
        },
        args,
    )?;
    let library_root = crate::clean::layout::PbfhoggLayout::library_runs(&args.paths.scratch_dir);
    std::fs::create_dir_all(&library_root)?;
    let state = crate::clean::layout::ProjectState::new(project_root);
    std::fs::create_dir_all(state.root())?;
    let report = run::verify(
        harness,
        scenario,
        &datasets,
        &run::Places {
            library_root: &library_root,
            fifo_dir: state.root(),
        },
    )?;
    // No stop check here: `verify` made the last one before a passing run's
    // removal, which commits the verdict, so a stop after it never turns a
    // removed run into an interrupted one.
    for line in &report.diagnostics {
        output::run_msg(&format!("diagnostic {line}"));
    }
    if report.passed {
        let count = report.results.len();
        output::result_msg(&format!(
            "library {}: passed, {}, {}",
            scenario.id,
            output::count(count, "result"),
            output::format_duration(report.wall)
        ));
        return Ok(());
    }
    for line in &report.failures {
        output::error(line);
    }
    output::error(&format!(
        "evidence kept in {}",
        output::path(&report.run_dir)
    ));
    crate::artefacts::emit_clean_hint(output::run_msg);
    Err(DevError::Verify(format!(
        "library scenario {} failed ({})",
        scenario.id,
        output::count(report.failures.len(), "failure")
    )))
}

fn find_scenario<'a>(
    doc: &'a describe::Describe,
    id: &str,
) -> Result<&'a describe::Scenario, DevError> {
    doc.scenarios.iter().find(|s| s.id == id).ok_or_else(|| {
        let known: Vec<&str> = doc.scenarios.iter().map(|s| s.id.as_str()).collect();
        DevError::Refused(format!(
            "the library harness declares no scenario {id:?}; it declares {}",
            known.join(", ")
        ))
    })
}

/// Resolve every dataset role of `scenario` through `resolve` (its variant
/// to the file of the chosen dataset and snapshot).
fn bind_datasets(
    scenario: &describe::Scenario,
    resolve: impl Fn(&str) -> Result<PathBuf, ResolveError>,
    args: &RunArgs<'_>,
) -> Result<BTreeMap<String, PathBuf>, DevError> {
    let mut out = BTreeMap::new();
    for role in &scenario.roles {
        let describe::RoleSource::Dataset { variant } = &role.source else {
            continue;
        };
        let path = resolve(variant).map_err(|e| match e {
            ResolveError::SnapshotVariant { available, .. } => DevError::Config(format!(
                "scenario {} role {} reads variant {variant}, which snapshot {} of dataset {} \
                 does not have (it has {}); choose a snapshot that has it, or register the \
                 variant",
                scenario.id,
                role.id,
                args.snapshot,
                args.dataset,
                if available.is_empty() {
                    "none".to_owned()
                } else {
                    available.join(", ")
                }
            )),
            other => DevError::Config(format!(
                "scenario {} role {} (variant {variant}): {other}",
                scenario.id, role.id
            )),
        })?;
        out.insert(role.id.clone(), path);
    }
    Ok(out)
}

/// The listing, one string per line.
fn listing(doc: &describe::Describe) -> Vec<String> {
    use describe::RoleSource;
    let mut lines = Vec::new();
    for (i, scenario) in doc.scenarios.iter().enumerate() {
        if i > 0 {
            lines.push(String::new());
        }
        lines.push(format!(
            "{}  {}, deadline {}s",
            scenario.id,
            output::count(scenario.calls.len(), "call"),
            scenario.deadline_s,
        ));
        lines.push(format!("  {}", scenario.description));
        for call in &scenario.calls {
            lines.push(format!("  call       {:<24} {}", call.id, call.label));
        }
        for role in &scenario.roles {
            let source = match &role.source {
                RoleSource::Dataset { variant } => format!("dataset, variant {variant}"),
                RoleSource::Generated => "generated by setup".to_owned(),
                RoleSource::Output { owner } => format!("output of {owner}"),
                RoleSource::Scratch { owner } => format!("scratch of {owner}"),
            };
            lines.push(format!(
                "  role       {:<24} {} {source}",
                role.id,
                role.kind.name()
            ));
        }
        for assertion in &scenario.assertions {
            let scope = assertion
                .call
                .as_ref()
                .map_or_else(|| "scenario".to_owned(), |call| format!("call {call}"));
            lines.push(format!("  assertion  {:<24} {scope}", assertion.id));
        }
    }
    lines
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
