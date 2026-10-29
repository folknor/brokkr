//! `brokkr library`: pbfhogg used as a library, through a harness that a
//! pbfhogg workspace member provides. The protocol between the two sides is
//! `docs/commands/library.md`; this module is brokkr's side of it.
//!
//! - [`json`]: the strict JSON every protocol document is read with.
//! - [`describe`]: the describe document and its validation.
//! - [`run`]: one verification run of a scenario and its verdict.
//! - [`artefact`]: the artefact tree and documents a run leaves.
//! - [`stream`]: each call's FIFO records judged against its document.
//! - [`publication`]: each call's publication effects held against its
//!   output roles on disk.
//! - [`seed`]: the seeds of seeded output roles, written and recognized.
//! - [`counterpart`]: each call's CLI counterpart replayed and compared.
//! - [`interval`]: the CLI's command interval read from a timed replay.
//! - [`prep`]: the dataset page-cache preparation before each arm of a
//!   library against CLI measurement.
//! - [`vs_cli`]: the `/cli` row's workload qualifier and the comparison view.
//! - [`digest`]: the `pbfhogg digest` report the comparison reads.
//! - [`certify`]: `--all`'s capability classification and its record.
//! - [`identity`]: what a measured process ran, in brokkr-json-v1.
//! - [`measured`]: `--bench`, `--hotpath` and `--alloc`, the measured run
//!   and its recording.
//! - [`profile`]: a profiled process's hotpath report and its validity.

mod artefact;
mod certify;
mod counterpart;
mod describe;
mod digest;
mod identity;
mod json;
mod interval;
mod measured;
mod prep;
mod profile;
mod publication;
mod run;
mod seed;
mod stream;
mod vs_cli;

pub(crate) use describe::is_capability;
pub(crate) use json::canonical_difference;

/// The library against CLI comparison a stored measurement group holds,
/// one line each, for `brokkr results <uuid>`; `None` for a group without
/// a `/cli` row.
pub(crate) fn cli_comparison(rows: &[crate::db::StoredRow]) -> Option<Vec<String>> {
    vs_cli::from_rows(rows).map(|c| c.lines())
}

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::build::RustProfile;
use crate::config::HarnessConfig;
use crate::datasets::error::ResolveError;
use crate::datasets::resolve::{self, SnapshotRef};
use crate::error::DevError;
use crate::output;
use crate::pbfhogg::binary::{LibraryHarness, PbfhoggBinary};

/// The config table the harness build comes from, for messages.
const TABLE: &str = "[pbfhogg.library.harness]";

/// What `brokkr library` was asked.
pub(crate) struct LibraryArgs<'a> {
    pub(crate) project_root: &'a Path,
    /// The tree the harness is built from: the checkout, or the `--commit`
    /// worktree.
    pub(crate) build_root: &'a Path,
    pub(crate) harness: &'a HarnessConfig,
    /// `[pbfhogg.library.require]`, which `--all` certifies against.
    pub(crate) require: Option<&'a BTreeMap<String, String>>,
    /// The files composed into the config, so a certification records
    /// whether the tree built was clean under the measurement policy.
    pub(crate) config_files: &'a [PathBuf],
    /// `--commit REF`, when given.
    pub(crate) commit: Option<&'a str>,
    /// `--profile`/`--debug`/`--release`, when given.
    pub(crate) profile: Option<RustProfile>,
    /// What to run; `None` lists the scenarios.
    pub(crate) run: Option<RunArgs<'a>>,
    /// `--bench N`, `--hotpath N` or `--alloc N`: measure the one scenario
    /// `run` names.
    pub(crate) measure: Option<MeasureArgs<'a>>,
}

/// A measurement and what its session is made from.
pub(crate) struct MeasureArgs<'a> {
    /// Bench, hotpath or alloc, with its process count; never run mode.
    pub(crate) mode: crate::measure::MeasureMode,
    pub(crate) force: bool,
    pub(crate) dev_config: &'a crate::config::DevConfig,
    pub(crate) project_root: &'a crate::project::ProjectRoot,
    pub(crate) build_root: &'a crate::project::BuildRoot,
    pub(crate) invocation: &'a crate::db::RunInvocation,
}

/// Which scenarios a run covers.
pub(crate) enum Target<'a> {
    /// `SCENARIO`: one, as a verification.
    One(&'a str),
    /// `--all`: every one, then the certification.
    All,
}

/// `brokkr library SCENARIO|--all`: what to run, and where its inputs come
/// from.
pub(crate) struct RunArgs<'a> {
    pub(crate) target: Target<'a>,
    pub(crate) dataset: &'a str,
    pub(crate) snapshot: SnapshotRef,
    pub(crate) paths: &'a crate::config::ResolvedPaths,
}

/// `brokkr library [SCENARIO | --all]`: build the harness, read its describe
/// document, then list the scenarios, run one, or certify them all.
pub(crate) fn library(args: &LibraryArgs<'_>) -> Result<(), DevError> {
    // The requirement list is taken once, before anything is built: what is
    // printed, classified, hashed and stored is this one value.
    let require = match &args.run {
        Some(RunArgs {
            target: Target::All,
            ..
        }) => Some(certify::Requirements::new(
            args.require
                .ok_or_else(|| {
                    DevError::Config(
                        "brokkr library --all certifies against [pbfhogg.library.require], \
                         which brokkr.toml does not have: list each required capability id \
                         with its meaning there, or leave the table empty to require none \
                         (see `brokkr man library`, Certification)"
                            .into(),
                    )
                })?
                .clone(),
        )),
        _ => None,
    };
    let _lock = crate::context::reenter_command_lock()?;
    // One window for the whole command: the build, describe and run guards
    // nest inside it, so a stop between them or during parsing is seen, not
    // taken as the default action past the tree policy's restore.
    let _interrupts = crate::shutdown::SigtermGuard::install();
    // A measurement's session first: a dirty tree is refused (or, with
    // `--force`, unstored) before anything is built, as for every measured
    // command.
    let session = match (&args.measure, &args.run) {
        (Some(measure), Some(run)) => Some(session(measure, run.paths)?),
        _ => None,
    };
    let profiled = args.measure.as_ref().and_then(|m| m.mode.profiled());
    if let Some(mode) = profiled {
        mode.print_note();
    }
    let profile = args
        .profile
        .clone()
        .or_else(|| args.harness.configured_profile(TABLE))
        .unwrap_or(RustProfile::Release);
    // A run also needs the CLI its counterparts replay against, built
    // before either handle exists; the listing does not. A profiled
    // measurement builds the harness with its instrumentation feature.
    let (harness, cli) = LibraryHarness::build(
        args.harness,
        profile.clone(),
        profiled,
        args.run.is_some(),
        args.build_root,
        args.project_root,
    )?;
    let bytes = harness.describe(describe::DESCRIBE_LIMIT)?;
    let doc = describe::parse(&bytes).map_err(|why| {
        DevError::ContractViolation(format!("the library harness's describe document: {why}"))
    })?;
    if crate::shutdown::should_unwind() {
        return Err(DevError::Interrupted);
    }
    let Some(run) = &args.run else {
        return list(&doc);
    };
    let cli = cli
        .as_ref()
        .ok_or_else(|| DevError::Internal("a library run was built without its CLI".into()))?;
    match (&run.target, &require) {
        (Target::One(id), _) => match (&session, &args.measure) {
            (Some(session), Some(measure_args)) => {
                let scenario = find_scenario(&doc, id)?;
                if let Some(mode) = profiled {
                    profiling_allowed(&doc, scenario, mode)?;
                }
                let (datasets, _) = bind_datasets(scenario, run, args.project_root)?;
                let (library_root, state_root) = places(run, args.project_root)?;
                measured::measure(
                    &measured::Measurement {
                        harness: &harness,
                        cli,
                        session,
                        mode: measure_args.mode,
                        dataset: run.dataset,
                        project_root: args.project_root,
                    },
                    scenario,
                    &datasets,
                    &run::Places {
                        library_root: &library_root,
                        fifo_dir: &state_root,
                    },
                )
            }
            _ => run_one(&harness, cli, &doc, run, id, args.project_root),
        },
        (Target::All, Some(require)) => certify_all(
            &Handles {
                harness: &harness,
                cli,
                profile: &profile,
            },
            &doc,
            run,
            require,
            args,
        ),
        (Target::All, None) => Err(DevError::Internal(
            "a library certification lost its requirement list".into(),
        )),
    }
}

/// Refuse to profile a scenario its describe does not mark `hotpath`: the
/// profile is the whole process's, so only a scenario declared to be one
/// call and no other library work has a profile that is its call's.
fn profiling_allowed(
    doc: &describe::Describe,
    scenario: &describe::Scenario,
    mode: crate::measure::ProfiledMode,
) -> Result<(), DevError> {
    if scenario.hotpath {
        return Ok(());
    }
    let capable: Vec<&str> = doc
        .scenarios
        .iter()
        .filter(|s| s.hotpath)
        .map(|s| s.id.as_str())
        .collect();
    let flag = match mode {
        crate::measure::ProfiledMode::Hotpath => "--hotpath",
        crate::measure::ProfiledMode::Alloc => "--alloc",
    };
    Err(DevError::Refused(format!(
        "scenario {} is not declared hotpath in the describe document, so {flag} cannot \
         profile it (the profile is the whole process's); {}",
        scenario.id,
        if capable.is_empty() {
            "no scenario is declared hotpath".to_owned()
        } else {
            format!("declared hotpath: {}", capable.join(", "))
        }
    )))
}

/// The measurement session of `--bench`, `--hotpath` or `--alloc`: the
/// results and sidecar stores, the tree's cleanliness under the measurement
/// policy, the recorded mode, the invocation and captured environment every
/// row carries.
fn session(
    bench: &MeasureArgs<'_>,
    paths: &crate::config::ResolvedPaths,
) -> Result<crate::harness::BenchHarness, DevError> {
    let lock = crate::context::reenter_command_lock()?;
    let session = crate::harness::BenchHarness::new_with_lock(
        lock,
        paths,
        &bench.dev_config.included_config_files,
        bench.build_root,
        bench.project_root,
        crate::project::Project::Pbfhogg,
        bench.force,
        None,
    )?
    .with_invocation(bench.invocation.clone())
    .with_measure_mode(bench.mode.recorded())
    .with_env_kv(crate::config::captured_env_pairs(bench.dev_config));
    session.note_unneeded_force(bench.force);
    Ok(session)
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
    cli: &PbfhoggBinary,
    doc: &describe::Describe,
    run: &RunArgs<'_>,
    id: &str,
    project_root: &Path,
) -> Result<(), DevError> {
    let scenario = find_scenario(doc, id)?;
    let (datasets, _) = bind_datasets(scenario, run, project_root)?;
    let (library_root, state_root) = places(run, project_root)?;
    let report = run::verify(
        harness,
        cli,
        scenario,
        &datasets,
        &run::Places {
            library_root: &library_root,
            fifo_dir: &state_root,
        },
        None,
        &mut None,
        &mut None,
    )?;
    // No stop check here: `verify` made the last one before a passing run's
    // removal, which commits the verdict, so a stop after it never turns a
    // removed run into an interrupted one.
    if show_report(scenario, &report) {
        return Ok(());
    }
    Err(DevError::Verify(format!(
        "library scenario {} failed ({})",
        scenario.id,
        output::count(report.failures.len(), "failure")
    )))
}

/// Make the library run root and the state root a run needs.
fn places(run: &RunArgs<'_>, project_root: &Path) -> Result<(PathBuf, PathBuf), DevError> {
    let library_root = crate::clean::layout::PbfhoggLayout::library_runs(&run.paths.scratch_dir);
    std::fs::create_dir_all(&library_root)?;
    let state = crate::clean::layout::ProjectState::new(project_root);
    std::fs::create_dir_all(state.root())?;
    Ok((library_root, state.root().to_path_buf()))
}

/// Print a run's report: on pass its stream, diagnostic and counterpart
/// lines and one result line; on fail every failure, the kept directory and
/// the clean hint. Whether it passed.
fn show_report(scenario: &describe::Scenario, report: &run::Report) -> bool {
    for line in &report.streams {
        output::run_msg(&format!("stream {line}"));
    }
    for line in &report.diagnostics {
        output::run_msg(&format!("diagnostic {line}"));
    }
    if let Some(line) = &report.timing {
        output::run_msg(&format!("timing {line}"));
    }
    // A failing result's detail is printed with the failures; a passing CLI
    // result's (its counts and declared policy differences) here.
    for (call, result) in &report.results {
        if result.id == counterpart::CLI_RESULT && result.status == artefact::Status::Pass {
            for line in result.detail.lines() {
                output::run_msg(&format!(
                    "cli {} {}",
                    call.as_deref().unwrap_or("-"),
                    run::printable(line)
                ));
            }
        }
    }
    if report.passed {
        let count = report.results.len();
        output::result_msg(&format!(
            "library {}: passed, {}, {}",
            scenario.id,
            output::count(count, "result"),
            output::format_duration(report.wall)
        ));
        return true;
    }
    for line in &report.failures {
        output::error(line);
    }
    output::error(&format!(
        "evidence kept in {}",
        output::path(&report.run_dir)
    ));
    crate::artefacts::emit_clean_hint(output::run_msg);
    false
}

/// The built executables a certification runs and records.
struct Handles<'a> {
    harness: &'a LibraryHarness,
    cli: &'a PbfhoggBinary,
    profile: &'a RustProfile,
}

/// `brokkr library --all`: print the requirement list, bind every scenario
/// before the first runs, run each once in describe order whatever the
/// others came to, classify every required capability, then write the
/// certification record. A stop seen before the record is written
/// interrupts, and no record is written; after that last check the
/// certification finishes.
fn certify_all(
    handles: &Handles<'_>,
    doc: &describe::Describe,
    run: &RunArgs<'_>,
    require: &certify::Requirements,
    args: &LibraryArgs<'_>,
) -> Result<(), DevError> {
    let digest = require.digest();
    output::run_msg(&format!(
        "library --all: {} against {}, {digest}",
        output::count(doc.scenarios.len(), "scenario"),
        output::count(require.list().len(), "required capability"),
    ));
    for (id, meaning) in require.list() {
        output::run_msg(&format!("  requires {id}: {meaning}"));
    }

    // Every binding first, so a dataset that lacks a variant refuses the
    // sweep before anything runs.
    let mut bound = Vec::new();
    for scenario in &doc.scenarios {
        bound.push((scenario, bind_datasets(scenario, run, args.project_root)?));
    }
    // So is what the record needs about the tree, the host and the builds.
    let git = crate::git::collect_with_config_paths(args.build_root, args.config_files)?;
    let commit = crate::git::resolve_commit(args.build_root, "HEAD")?;
    let hostname = crate::config::hostname()?;
    let uuid = crate::db::generate_uuid()?;
    let unix_time_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (library_root, state_root) = places(run, args.project_root)?;
    let total = bound.len();
    let attempts = sweep(
        handles,
        bound,
        &run::Places {
            library_root: &library_root,
            fifo_dir: &state_root,
        },
    )?;

    let verdicts = certify::classify(require, &attempts);
    let certified = certify::certified(&attempts, &verdicts);
    // The finalization boundary: past it the certification is written and
    // reported whatever arrives.
    if crate::shutdown::should_unwind() {
        return Err(interrupted(total, total));
    }
    for verdict in &verdicts {
        let line = certify::verdict_line(verdict);
        if verdict.class == certify::Class::Passed {
            output::run_msg(&line);
        } else {
            output::error(&line);
        }
    }
    let not_required = certify::not_required(require, &attempts);
    if !not_required.is_empty() {
        output::run_msg(&format!(
            "covered, not required: {}",
            not_required.join(", ")
        ));
    }

    let record = certify::record(
        &certify::Provenance {
            uuid: &uuid,
            unix_time_s,
            hostname: &hostname,
            commit: commit.as_str(),
            clean: git.is_clean,
            commit_ref: args.commit,
            harness: args.harness,
            profile: handles.profile.label(),
            harness_xxh128: handles.harness.digest(),
            cli_xxh128: handles.cli.digest(),
            dataset: run.dataset,
            snapshot: run.snapshot.as_str(),
        },
        require,
        &attempts,
        &verdicts,
    );
    let path = write_record(args.project_root, &uuid, &record)?;

    let passed_scenarios = attempts.iter().filter(|a| a.passed).count();
    let tally = |class: certify::Class| verdicts.iter().filter(|v| v.class == class).count();
    let summary = format!(
        "{passed_scenarios}/{total} scenarios passed; capabilities: {} passed, {} failed, {} \
         unsupported of {}; {digest}{}; record {}",
        tally(certify::Class::Passed),
        tally(certify::Class::Failed),
        tally(certify::Class::Unsupported),
        verdicts.len(),
        if git.is_clean {
            ""
        } else {
            "; the tree was dirty"
        },
        output::path(&path)
    );
    if certified {
        output::result_msg(&format!("library --all: certified, {summary}"));
        Ok(())
    } else {
        output::error(&format!("library --all: not certified, {summary}"));
        Err(DevError::Verify("library --all: not certified".into()))
    }
}

/// Publish the certification record as `<uuid>.json` (atomically: never a
/// partial file). A failure fails the command: the record may or may not
/// exist then.
fn write_record(
    project_root: &Path,
    uuid: &str,
    record: &serde_json::Value,
) -> Result<PathBuf, DevError> {
    let dir = crate::clean::layout::ProjectState::new(project_root).library_certifications();
    let path = dir.join(format!("{uuid}.json"));
    let written = (|| -> std::io::Result<()> {
        let mut bytes = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        std::fs::create_dir_all(&dir)?;
        crate::atomic_write::replace(&path, &bytes)
    })();
    written.map(|()| path.clone()).map_err(|e| {
        DevError::Verify(format!(
            "library --all: the certification record {} could not be written ({e}); it may or \
             may not exist, so this certification is not recorded",
            output::path(&path)
        ))
    })
}

/// A scenario with its bound dataset roles: role id to path, and the
/// bindings the record keeps.
type Bound<'a> = (
    &'a describe::Scenario,
    (BTreeMap<String, PathBuf>, Vec<certify::Binding>),
);

/// Run each bound scenario once, in order, whatever the others came to: a
/// run that did not come to a verdict is that scenario's failure. A stop
/// seen before a run, or during one, interrupts the sweep.
fn sweep<'a>(
    handles: &Handles<'_>,
    bound: Vec<Bound<'a>>,
    places: &run::Places<'_>,
) -> Result<Vec<certify::Attempt<'a>>, DevError> {
    let total = bound.len();
    let mut attempts: Vec<certify::Attempt<'a>> = Vec::new();
    for (i, (scenario, (datasets, bindings))) in bound.into_iter().enumerate() {
        if crate::shutdown::should_unwind() {
            return Err(interrupted(i, total));
        }
        output::run_msg(&format!("scenario {}/{total}: {}", i + 1, scenario.id));
        let verdict = run::verify(
            handles.harness,
            handles.cli,
            scenario,
            &datasets,
            places,
            None,
            &mut None,
            &mut None,
        );
        let attempt = match verdict {
            Ok(report) => {
                show_report(scenario, &report);
                certify::Attempt::of_report(scenario, bindings, &report)
            }
            Err(DevError::Interrupted) => return Err(interrupted(i, total)),
            // A stop can arrive as the harness launch's own error.
            Err(_) if crate::shutdown::should_unwind() => return Err(interrupted(i, total)),
            Err(e) => {
                let why = format!("it did not come to a verdict: {e}");
                output::error(&format!("library {}: {why}", scenario.id));
                certify::Attempt::errored(scenario, bindings, why)
            }
        };
        attempts.push(attempt);
    }
    Ok(attempts)
}

/// A stop seen with `done` of `total` scenarios finished: reported, no
/// record written.
fn interrupted(done: usize, total: usize) -> DevError {
    output::run_msg(&format!(
        "library --all: interrupted with {done} of {total} scenarios finished; no \
         certification recorded"
    ));
    DevError::Interrupted
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

/// Resolve every dataset role of `scenario` (its variant to the file of the
/// chosen dataset and snapshot): role id to path, and each binding with the
/// configured digest of the entry it came from.
fn bind_datasets(
    scenario: &describe::Scenario,
    run: &RunArgs<'_>,
    project_root: &Path,
) -> Result<(BTreeMap<String, PathBuf>, Vec<certify::Binding>), DevError> {
    bind_with(
        scenario,
        |variant| {
            resolve::resolve_snapshot_pbf_entry(
                run.dataset,
                &run.snapshot,
                variant,
                run.paths,
                project_root,
            )
            .map(|resolved| {
                (
                    resolved.path,
                    resolved.entry.xxhash.as_ref().map(ToString::to_string),
                )
            })
        },
        run.dataset,
        &run.snapshot,
    )
}

fn bind_with(
    scenario: &describe::Scenario,
    resolve: impl Fn(&str) -> Result<(PathBuf, Option<String>), ResolveError>,
    dataset: &str,
    snapshot: &SnapshotRef,
) -> Result<(BTreeMap<String, PathBuf>, Vec<certify::Binding>), DevError> {
    let mut out = BTreeMap::new();
    let mut bindings = Vec::new();
    for role in &scenario.roles {
        let describe::RoleSource::Dataset { variant } = &role.source else {
            continue;
        };
        let (path, xxhash) = resolve(variant).map_err(|e| match e {
            ResolveError::SnapshotVariant { available, .. } => DevError::Config(format!(
                "scenario {} role {} reads variant {variant}, which snapshot {snapshot} of \
                 dataset {dataset} does not have (it has {}); choose a snapshot that has it, or \
                 register the variant",
                scenario.id,
                role.id,
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
        out.insert(role.id.clone(), path.clone());
        bindings.push(certify::Binding {
            role: role.id.clone(),
            variant: variant.clone(),
            path,
            xxhash,
        });
    }
    Ok((out, bindings))
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
            "{}  {}, deadline {}s{}{}",
            scenario.id,
            output::count(scenario.calls.len(), "call"),
            scenario.deadline_s,
            if scenario.hotpath { ", hotpath" } else { "" },
            if scenario.isolation {
                ", isolation"
            } else {
                ""
            },
        ));
        lines.push(format!("  {}", scenario.description));
        for call in &scenario.calls {
            let mut line = format!("  call       {:<24} {}", call.id, call.label);
            if !call.stream.is_empty() {
                line.push_str(&format!(" (stream: {})", call.stream.join(", ")));
            }
            lines.push(line);
            if let Some(cli) = &call.cli {
                let argv: Vec<String> = cli.argv.iter().map(describe::Token::show).collect();
                let mut notes = Vec::new();
                if cli.scratch {
                    notes.push("scratch".to_owned());
                }
                if let describe::Expect::Refusal { .. } = cli.expect {
                    notes.push(format!("expects a {}", cli.expect.show()));
                }
                match cli.overwrite {
                    Some(true) => notes.push("overwrite".to_owned()),
                    Some(false) => notes.push("no overwrite".to_owned()),
                    None => {}
                }
                let notes = if notes.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", notes.join(", "))
                };
                lines.push(format!("    cli    pbfhogg {}{notes}", argv.join(" ")));
                for difference in &cli.policy_differences {
                    lines.push(format!("    policy {difference}"));
                }
            }
        }
        for role in &scenario.roles {
            let source = match &role.source {
                RoleSource::Dataset { variant } => format!("dataset, variant {variant}"),
                RoleSource::Generated => "generated by setup".to_owned(),
                RoleSource::Output {
                    owner,
                    seeded: false,
                } => format!("output of {owner}"),
                RoleSource::Output {
                    owner,
                    seeded: true,
                } => format!("output of {owner}, seeded"),
                RoleSource::Scratch { owner } => format!("scratch of {owner}"),
            };
            lines.push(format!(
                "  role       {:<24} {} {source}",
                role.id,
                role.kind.name()
            ));
        }
        for assertion in &scenario.assertions {
            let mut scope = assertion
                .call
                .as_ref()
                .map_or_else(|| "scenario".to_owned(), |call| format!("call {call}"));
            if !assertion.covers.is_empty() {
                scope.push_str(&format!(" (covers: {})", assertion.covers.join(", ")));
            }
            lines.push(format!("  assertion  {:<24} {scope}", assertion.id));
        }
    }
    lines
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
