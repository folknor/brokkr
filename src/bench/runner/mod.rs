//! `brokkr bench NAME`: a script bench's runs, one per arm, end to end.
//!
//! Before any attempt, the bench's params are bound against the resolved
//! view (`super::params`, `super::view`) into every arm they fan out to,
//! every arm's bound files reserved for reading, the script's `preflight`
//! run for every arm when it declares one (`preflight`), and the bound files
//! digested; a refusal there stores nothing, for any arm.
//!
//! Each arm is then one attempt with one document
//! (`reference/bench-store.md`), run in order; one that fails does not stop
//! the rest, a stop, a publication failure or (without `--commit`) HEAD moving
//! off the invocation's commit ends the invocation. An arm is: the build the
//! bench's TOML asks for (host features folded in), in the `--commit` worktree
//! when given, failing the arm when the tree built is no longer at that
//! commit once it is done, then iterations, each a fresh Lua state
//! over one frozen compiled script, an owner lease and its children on the
//! resource protocol (`reference/bench-resources.md`), then one publication
//! to the per-user bench store, local or the configured results server.
//! Whatever the attempt reached is published, a failed build or an
//! interrupted iteration included; the command then fails if any arm did.

mod host;
mod preflight;
#[cfg(test)]
mod run_tests;
mod stored_config;
mod validate;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::lua;
use super::params;
use super::store;
use super::view;
use crate::config::bench::{Bench, BenchScript, Domain};
use crate::error::DevError;
use crate::output;
use crate::project::Detection;

/// A parsed `brokkr bench` invocation.
pub(crate) struct BenchArgs {
    /// The bench to run; `None` lists the benches.
    pub(crate) name: Option<String>,
    /// Run the commit's build, in a persistent worktree.
    pub(crate) commit: Option<String>,
    pub(crate) iterations: std::num::NonZeroU32,
    /// `--hotpath`: build with the bench's `hotpath_features` and require
    /// every marked call to capture.
    pub(crate) hotpath: bool,
    /// `--alloc`: build with the bench's `alloc_features` and require every
    /// marked call to capture. Exclusive with `hotpath`.
    pub(crate) alloc: bool,
    /// Everything after NAME, unparsed: the bench's own params.
    pub(crate) params: Vec<String>,
    /// `--dry-run`: bind and validate every arm, resolve the commit, check
    /// the inputs and their pins, then stop before anything is built.
    pub(crate) dry_run: bool,
}

/// The stdout and stderr preview a script sees of each child, in bytes.
pub(crate) const PREVIEW_BYTES: usize = 1024 * 1024;

pub(crate) fn run(
    args: &BenchArgs,
    detection: &Detection,
    invocation: &crate::db::RunInvocation,
) -> Result<(), DevError> {
    let benches = &detection.config.benches;
    let Some(name) = args.name.as_deref() else {
        list(benches);
        return Ok(());
    };
    let bench = find(benches, name)?;
    let measurement = measurement_of(args, bench)?;
    let given = params::parse(bench, &args.params)?;
    let hostname = crate::config::hostname()?;
    // What the run stores is what the view derives from: the composed table,
    // the hostname and the live project root.
    let view = view::build(
        detection.config_sources.composed(),
        &hostname,
        detection.project_root.as_path(),
    );
    if given.help {
        describe(bench, detection, &given, &view);
        return Ok(());
    }
    // Every refusal from here to the first attempt is before anything is
    // stored, and covers every arm: an invocation is refused whole.
    let mut arms = params::bind(bench, &given, &view)?;
    let frozen = freeze(bench)?;
    let (config, labels, validated) = validate(name, bench, &view, &frozen, &arms)?;
    if args.dry_run {
        return dry_run(name, args, measurement, bench, &frozen, detection, &mut arms, &labels);
    }
    // Opened, or for a results server probed, before anything is built or
    // run and before a stop is caught: a run with nowhere to go is never
    // measured.
    let container = iteration_container(detection, &hostname)?;
    let mut destination = Destination::open(&detection.config)?;
    let _inputs_held = hold_inputs(&arms)?;
    let keep = crate::context::worktree_keep(&detection.config)?;
    let tree = crate::tree_policy::TreePolicy::of(&detection.config);
    let Prepared { commit, modules } = preflight_and_digest(
        name, args, detection, &frozen, &mut arms, &labels, &config, validated,
    )?;
    // The whole invocation from here, worktree setup and publication
    // included: a stop that lands between two children, during the build or
    // while publishing is recorded rather than taking the default action, so
    // the attempt is still published (interrupted, unless something it
    // observed decided it).
    let _interrupts = crate::shutdown::SigtermGuard::install();
    // A preflight may have run anything: the first arm is admitted as every
    // later one is.
    if frozen.script.has_preflight() {
        admit_next(name, args, detection, &commit)?;
    }
    let argv = crate::history::redact_secret_argv(&invocation.argv);
    let total = arms.len();
    let mut failed: Vec<&str> = Vec::new();
    for (index, bound) in arms.iter().enumerate() {
        let label = labels[index].as_str();
        if index > 0
            && let Err(error) = admit_next(name, args, detection, &commit)
        {
            not_run(name, &labels[index..]);
            return Err(error);
        }
        if total > 1 {
            output::bench_msg(&format!("arm {}/{total}: {label}", index + 1));
        }
        let shown = if label.is_empty() {
            name.to_owned()
        } else {
            format!("{name} ({label})")
        };
        let attempt = Attempt {
            detection,
            bench,
            frozen: &frozen,
            bound,
            args: bound.args(),
            inputs: bound.inputs(),
            config: std::sync::Arc::clone(&config),
            committed: args.commit.is_some(),
            hostname: &hostname,
            container: &container,
            iterations: args.iterations.get(),
            argv: argv.clone(),
            measurement,
            preflight_modules: &modules[index],
        };
        let started = Instant::now();
        let document = match attempt.run(&commit, &tree, keep) {
            Ok(document) => document,
            Err(error) => {
                not_run(name, &labels[index..]);
                return Err(error);
            }
        };
        if let Err(error) = destination.publish(&document) {
            // The cause may then be stored nowhere: say it here.
            if let store::RunOutcome::Failed { reason } = &document.outcome {
                output::error(&format!("bench {shown} failed: {reason}"));
            }
            not_run(name, &labels[index + 1..]);
            return Err(error);
        }
        report(&document, &shown, &destination.shown(), started);
        match &document.outcome {
            store::RunOutcome::Completed => {}
            store::RunOutcome::Interrupted => {
                not_run(name, &labels[index + 1..]);
                return Err(DevError::Interrupted);
            }
            // An arm that failed, its build or worktree included, does not
            // stop the rest.
            store::RunOutcome::Failed { reason } => {
                output::error(&format!("bench {shown} failed: {reason}"));
                failed.push(label);
            }
        }
    }
    summary(name, total, &failed)
}

/// Where this host's iterations make their directories:
/// `<scratch>/bench-iterations`, on the drive the typed commands write to.
/// Refused when it lies strictly inside the project state root anywhere but
/// the state root's own `bench-iterations` (a scratch of `.brokkr` itself):
/// every other directory there is one brokkr wipes or prunes by fixed name,
/// outside the reservation gate (`reference/bench-resources.md`, Placement
/// and cleanup). Compared lexically, `..` folded: a diagnostic for the one
/// misplacement cheap to detect exactly, not the placement guarantee.
fn iteration_container(detection: &Detection, hostname: &str) -> Result<PathBuf, DevError> {
    let root = detection.project_root.as_path();
    let scratch = crate::config::resolve_paths(&detection.config, hostname, root).scratch_dir;
    let container = fold(&crate::clean::layout::bench_iterations_container(&scratch));
    let state = crate::clean::layout::ProjectState::new(root);
    let (state_root, allowed) = (fold(state.root()), fold(&state.bench_iterations_dir()));
    if container != allowed && container.starts_with(&state_root) {
        return Err(DevError::Config(format!(
            "host {hostname}'s scratch {} puts bench iterations at {}, inside brokkr's state \
             root {}, where brokkr removes directories by fixed name: point scratch elsewhere, \
             or at {} itself",
            scratch.display(),
            container.display(),
            state_root.display(),
            state_root.display()
        )));
    }
    Ok(container)
}

/// `path` with `.` and `..` folded lexically.
fn fold(path: &Path) -> PathBuf {
    let mut folded = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                folded.pop();
            }
            other => folded.push(other),
        }
    }
    folded
}

/// The bench named `name`, or a refusal naming the ones declared.
fn find<'a>(benches: &'a BTreeMap<String, Bench>, name: &str) -> Result<&'a Bench, DevError> {
    benches.get(name).ok_or_else(|| {
        DevError::Refused(if benches.is_empty() {
            format!("no bench named {name}: this project's brokkr.toml declares none")
        } else {
            format!(
                "no bench named {name}; declared: {}",
                benches.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    })
}

/// What the invocation measures: `--hotpath` (`--alloc`) only for a bench
/// that names the features instrumenting its build for it.
fn measurement_of(args: &BenchArgs, bench: &Bench) -> Result<store::Measurement, DevError> {
    let (measurement, key) = match (args.hotpath, args.alloc) {
        (false, false) => return Ok(store::Measurement::Timing),
        (true, true) => {
            return Err(DevError::Refused(
                "--hotpath and --alloc exclude each other: a run measures one".into(),
            ));
        }
        (true, false) => (store::Measurement::Hotpath, "hotpath_features"),
        (false, true) => (store::Measurement::Alloc, "alloc_features"),
    };
    if profile_features(bench, measurement).is_empty() {
        return Err(DevError::Refused(format!(
            "bench {0} cannot be profiled with {1}: [bench.{0}] sets no {key}, the features \
             that instrument its build",
            bench.name,
            measurement.flag().unwrap_or_default()
        )));
    }
    Ok(measurement)
}

/// The features `measurement` adds to the bench's build: none for timing.
fn profile_features(bench: &Bench, measurement: store::Measurement) -> &[String] {
    match measurement {
        store::Measurement::Timing => &[],
        store::Measurement::Hotpath => &bench.hotpath_features,
        store::Measurement::Alloc => &bench.alloc_features,
    }
}

/// The features a `measurement` build of `bench` asks cargo for: the
/// bench's, then the ones `measurement` adds.
fn build_features(
    config: &crate::config::DevConfig,
    bench: &Bench,
    measurement: store::Measurement,
) -> Vec<String> {
    let mut features = crate::command_args::resolve_features(config, &bench.features);
    for feature in profile_features(bench, measurement) {
        if !features.contains(feature) {
            features.push(feature.clone());
        }
    }
    features
}

/// How the invocation ends once every arm ran: each failed arm's cause was
/// printed as it failed, so this names them only.
fn summary(name: &str, total: usize, failed: &[&str]) -> Result<(), DevError> {
    match failed.len() {
        0 => Ok(()),
        _ if total == 1 => Err(DevError::Reported(format!("bench {name} failed"))),
        n => Err(DevError::Reported(format!(
            "{n} of {total} arms of bench {name} failed: {}",
            failed.join("; ")
        ))),
    }
}

/// Reserve every arm's bound files for reading, before any preflight can
/// look at them and before any is hashed, held until the last arm ends (the
/// returned hold), so no lease writes one between its digest and the last
/// iteration.
fn hold_inputs(arms: &[params::Bound]) -> Result<Option<crate::reservations::ReadHold>, DevError> {
    let union = params::union_inputs(arms);
    if union.is_empty() {
        return Ok(None);
    }
    let paths: Vec<&Path> = union.iter().map(PathBuf::as_path).collect();
    Ok(Some(crate::reservations::hold_reads(&paths)?))
}

/// What every arm shares once its preflight passed and its inputs are
/// hashed.
struct Prepared {
    commit: crate::git::Commit,
    /// Each arm's library modules its preflight asked for.
    modules: Vec<BTreeSet<String>>,
}

/// The invocation's commit and each arm's modules, those its `validate()`
/// asked for (`validated`) joined by its preflight's, once every arm's
/// preflight passed and its inputs are hashed. An unknown revision is
/// refused before any project code runs.
#[allow(clippy::too_many_arguments)]
fn preflight_and_digest(
    name: &str,
    args: &BenchArgs,
    detection: &Detection,
    frozen: &Frozen,
    arms: &mut [params::Bound],
    labels: &[String],
    config: &std::sync::Arc<lua::Value>,
    validated: Vec<BTreeSet<String>>,
) -> Result<Prepared, DevError> {
    let commit = crate::git::resolve_commit(
        detection.build_root.as_path(),
        args.commit.as_deref().unwrap_or("HEAD"),
    )?;
    // A stop during a preflight is recorded, so its child is stopped with its
    // process group and the Lua it runs sees it; one that lands as the
    // window closes is read in the same step that closes it.
    let guard = crate::shutdown::SigtermGuard::install();
    let preflighted = preflight_arms(name, frozen, arms, labels, config, detection, &commit);
    if guard.leave() {
        return Err(DevError::Interrupted);
    }
    let modules = preflighted?
        .into_iter()
        .zip(validated)
        .map(|(mut preflight, validate)| {
            preflight.extend(validate);
            preflight
        })
        .collect();
    // Hashed under the default action, as ever: a stop during it ends the
    // invocation at once, nothing stored.
    params::digest_arms(
        arms,
        params::Depth::Every,
        detection.project_root.as_path(),
        &detection.config_sources,
    )?;
    Ok(Prepared { commit, modules })
}

/// `config()`'s table, the arm labels and each arm's `validate()` modules,
/// once every arm validated: a bench whose `validated` params have no
/// `validate()` to check them is refused first.
type Validated = (std::sync::Arc<lua::Value>, Vec<String>, Vec<BTreeSet<String>>);

fn validate(
    name: &str,
    bench: &Bench,
    view: &view::View,
    frozen: &Frozen,
    arms: &[params::Bound],
) -> Result<Validated, DevError> {
    if let Some(why) = super::unvalidated(bench, frozen.script.has_validate()) {
        return Err(DevError::Config(format!("[bench.{name}] {why}")));
    }
    let config = std::sync::Arc::new(view.lua());
    let labels = params::arm_labels(arms);
    let modules = validate_arms(name, frozen, arms, &labels, &config)?;
    Ok((config, labels, modules))
}

/// Run the script's `validate()`, when it declares one, for every arm in a
/// fresh State, as soon as the params bound: each arm's refusal collected,
/// and any refuses the invocation whole, before anything is opened, held,
/// built or stored. Returns each arm's library modules it asked for.
fn validate_arms(
    name: &str,
    frozen: &Frozen,
    arms: &[params::Bound],
    labels: &[String],
    config: &std::sync::Arc<lua::Value>,
) -> Result<Vec<BTreeSet<String>>, DevError> {
    if !frozen.script.has_validate() {
        return Ok(vec![BTreeSet::new(); arms.len()]);
    }
    // Pure Lua can spin: a stop is recorded, so the watcher interrupts it.
    let guard = crate::shutdown::SigtermGuard::install();
    let ran = (|| {
        let mut refusals = Vec::new();
        let mut modules = Vec::with_capacity(arms.len());
        for (index, bound) in arms.iter().enumerate() {
            if crate::shutdown::should_unwind() {
                return Err(DevError::Interrupted);
            }
            let host = validate::ValidateHost {
                config: std::sync::Arc::clone(config),
            };
            let (ran, _) =
                lua::run_hook(lua::Hook::Validate, &frozen.script, &bound.preflight_args(), host);
            modules.push(ran.modules);
            judge(lua::Hook::Validate, &shown(name, &labels[index]), ran.outcome, &mut refusals)?;
        }
        settle(modules, refusals)
    })();
    if guard.leave() {
        return Err(DevError::Interrupted);
    }
    ran
}

/// How one arm is named in a hook's refusal.
fn shown(name: &str, label: &str) -> String {
    match label {
        "" => format!("bench {name}"),
        label => format!("bench {name} ({label})"),
    }
}

/// One arm's hook outcome: a refusal or a script error joins `refusals`, so
/// every arm is judged; an interrupt or brokkr's own failure ends the hooks
/// there, since every later arm would meet the same machinery.
fn judge(
    hook: lua::Hook,
    shown: &str,
    outcome: lua::HookOutcome,
    refusals: &mut Vec<String>,
) -> Result<(), DevError> {
    let name = hook.name();
    match outcome {
        lua::HookOutcome::Passed => Ok(()),
        lua::HookOutcome::Interrupted => Err(DevError::Interrupted),
        lua::HookOutcome::Refused(why) => {
            refusals.push(format!("{shown}: {name} refused: {why}"));
            Ok(())
        }
        lua::HookOutcome::Broken(why) => {
            refusals.push(format!("{shown}: {name} script error: {why}"));
            Ok(())
        }
        lua::HookOutcome::Failed(why) => Err(DevError::Preflight(vec![format!(
            "{shown}: {name} could not run: {why}"
        )])),
    }
}

/// Every arm's hook run: a stop the last one did not see wins over its
/// refusals, and any refusal refuses the invocation whole.
fn settle(
    modules: Vec<BTreeSet<String>>,
    refusals: Vec<String>,
) -> Result<Vec<BTreeSet<String>>, DevError> {
    if crate::shutdown::should_unwind() {
        return Err(DevError::Interrupted);
    }
    if refusals.is_empty() {
        Ok(modules)
    } else {
        Err(DevError::Preflight(refusals))
    }
}

/// `brokkr bench NAME --dry-run`, once every arm bound and validated: the
/// commit resolved, every bound file's shape checked and a pinned one held
/// to its pin, each arm and what it would build named. Nothing is opened,
/// held, preflighted, built or stored: the preflight may launch programs, so
/// it is the run's to make.
#[allow(clippy::too_many_arguments)] // the bound invocation, each part its own
fn dry_run(
    name: &str,
    args: &BenchArgs,
    measurement: store::Measurement,
    bench: &Bench,
    frozen: &Frozen,
    detection: &Detection,
    arms: &mut [params::Bound],
    labels: &[String],
) -> Result<(), DevError> {
    let commit = crate::git::resolve_commit(
        detection.build_root.as_path(),
        args.commit.as_deref().unwrap_or("HEAD"),
    )?;
    params::digest_arms(
        arms,
        params::Depth::PinsOnly,
        detection.project_root.as_path(),
        &detection.config_sources,
    )?;
    let profile = bench.profile.clone().unwrap_or_else(|| {
        crate::runnables::build_profile(detection.config.bin.as_ref(), None)
    });
    let features = build_features(&detection.config, bench, measurement);
    output::dry_run_msg(&format!(
        "bench {name}: would build {} ({profile}, features [{}]) at {}{}, {} of {}",
        bench.target.as_deref().unwrap_or("[bin] default"),
        features.join(", "),
        crate::db::short_uuid(commit.as_str()),
        if args.commit.is_some() { " in a worktree" } else { "" },
        output::count(usize::try_from(args.iterations.get()).unwrap_or(usize::MAX), "iteration"),
        output::count(arms.len(), "arm"),
    ));
    if frozen.script.has_preflight() {
        output::dry_run_msg("preflight not run: it may launch programs, so the run makes it");
    }
    for (index, arm) in arms.iter().enumerate() {
        let shown: Vec<String> = arm
            .document()
            .into_iter()
            .map(|(param, bound)| {
                let keys: Vec<String> = bound.values.into_iter().map(|v| v.key).collect();
                format!("--{param} {}", keys.join(","))
            })
            .collect();
        let label = match labels.get(index).map(String::as_str) {
            None | Some("") => String::new(),
            Some(label) => format!(" ({label})"),
        };
        output::dry_run_msg(&format!(
            "arm {}/{}{label}: {}",
            index + 1,
            arms.len(),
            if shown.is_empty() {
                "no params".to_owned()
            } else {
                shown.join(" ")
            }
        ));
    }
    output::dry_run_msg("ok");
    Ok(())
}

/// Run the script's preflight, when it declares one, for every arm in a
/// fresh State, before anything is built: each arm's refusal is collected,
/// and any refuses the invocation whole, storing nothing. Passing means every
/// arm passed before the build began, not that what it checked still holds
/// when the arm runs. Returns each arm's library modules its preflight asked
/// for, which that arm's document records.
fn preflight_arms(
    name: &str,
    frozen: &Frozen,
    arms: &[params::Bound],
    labels: &[String],
    config: &std::sync::Arc<lua::Value>,
    detection: &Detection,
    commit: &crate::git::Commit,
) -> Result<Vec<BTreeSet<String>>, DevError> {
    if !frozen.script.has_preflight() {
        return Ok(vec![BTreeSet::new(); arms.len()]);
    }
    let mut since = host::Since::new(detection.build_root.as_path(), commit.clone());
    let mut refusals = Vec::new();
    let mut modules = Vec::with_capacity(arms.len());
    for (index, bound) in arms.iter().enumerate() {
        if crate::shutdown::should_unwind() {
            return Err(DevError::Interrupted);
        }
        let host = preflight::PreflightHost {
            config: std::sync::Arc::clone(config),
            cwd: detection.project_root.as_path().to_path_buf(),
            since,
        };
        let (ran, host) =
            lua::run_hook(lua::Hook::Preflight, &frozen.script, &bound.preflight_args(), host);
        since = host.since;
        modules.push(ran.modules);
        judge(lua::Hook::Preflight, &shown(name, &labels[index]), ran.outcome, &mut refusals)?;
    }
    settle(modules, refusals)
}

/// Whether the arm after a published one may start: not once a stop is
/// pending, and, without `--commit`, only while HEAD is still the commit
/// resolved for the invocation, since every arm builds the live tree and
/// records that sha. A move during an arm's own build fails that arm
/// (`Attempt::built_as_recorded`); this refuses the arms after it.
fn admit_next(
    name: &str,
    args: &BenchArgs,
    detection: &Detection,
    commit: &crate::git::Commit,
) -> Result<(), DevError> {
    if crate::shutdown::should_unwind() {
        return Err(DevError::Interrupted);
    }
    if args.commit.is_some() {
        return Ok(());
    }
    let head = crate::git::resolve_commit(detection.build_root.as_path(), "HEAD")?;
    if head != *commit {
        return Err(DevError::Refused(format!(
            "bench {name}: HEAD moved from {} to {} during the run, so the arms left would be \
             recorded under a commit they did not build",
            commit.as_str(),
            head.as_str()
        )));
    }
    Ok(())
}

/// Name the arms a stop or a refusal left unattempted.
fn not_run(name: &str, labels: &[String]) {
    if !labels.is_empty() {
        output::warn(&format!(
            "bench {name}: not run, so not stored: {}",
            labels.join("; ")
        ));
    }
}

/// The bare index: every bench, its target and where its script lives.
fn list(benches: &BTreeMap<String, Bench>) {
    if benches.is_empty() {
        output::bench_msg("this project's brokkr.toml declares no [bench.*] tables");
        return;
    }
    let mut lines = vec![format!(
        "{} declared:",
        output::count(benches.len(), "bench")
    )];
    for bench in benches.values() {
        let notes = &bench.annotations;
        let version = notes
            .version
            .value()
            .map_or_else(String::new, |v| format!(" v{v}"));
        let description = notes.description.value().map_or_else(String::new, |d| {
            format!(": {}", crate::config::bench_annotations::first_line(d))
        });
        lines.push(format!(
            "  {}{version} (target {}, {}){description}",
            bench.name,
            bench.target.as_deref().unwrap_or("[bin] default"),
            script_origin(bench)
        ));
    }
    lines.push(
        "run one with `brokkr bench <name>`; `brokkr bench <name> --help` describes it".into(),
    );
    output::bench_msg(&lines.join("\n"));
}

/// `brokkr bench NAME --help`: the bench's build settings and its params,
/// each `from` param with the keys it takes on this host, its parents chosen
/// from the command line, else their defaults.
fn describe(bench: &Bench, detection: &Detection, given: &params::Given, view: &view::View) {
    let mut lines = vec![
        format!("bench {}: {}", bench.name, script_origin(bench)),
        format!(
            "  target {}, profile {}, features [{}]{}",
            bench.target.as_deref().unwrap_or("[bin] default"),
            bench.profile.as_ref().map_or_else(
                || crate::runnables::build_profile(detection.config.bin.as_ref(), None).to_string(),
                ToString::to_string
            ),
            crate::command_args::resolve_features(&detection.config, &bench.features).join(", "),
            if bench.default_features {
                ""
            } else {
                ", no default features"
            }
        ),
    ];
    // The devs' notes under the bench's name, before its build settings.
    let notes = bench.annotations.help_lines();
    lines.splice(1..1, notes);
    if !bench.hotpath_features.is_empty() {
        lines.push(format!(
            "  profiled with --hotpath: features [{}] added",
            bench.hotpath_features.join(", ")
        ));
    }
    if !bench.alloc_features.is_empty() {
        lines.push(format!(
            "  profiled with --alloc: features [{}] added",
            bench.alloc_features.join(", ")
        ));
    }
    if bench.params.is_empty() {
        lines.push("  no params".into());
    }
    for (name, param) in &bench.params {
        let mut line = format!("  --{name}");
        match &param.domain {
            Domain::From(from) => line.push_str(&format!(" (a key under {})", from.as_str())),
            Domain::Choices(choices) => line.push_str(&format!(" ({})", choices.join(" | "))),
            Domain::Validated => line.push_str(" (checked by the script's validate())"),
            Domain::Free => line.push_str(" (any value)"),
        }
        if let Some(default) = &param.default {
            line.push_str(&format!(" [default {default}]"));
        }
        if param.required {
            line.push_str(" (required)");
        }
        if param.many {
            line.push_str(" (a collection)");
        }
        if let Some(help) = &param.help {
            line.push_str(&format!(": {help}"));
        }
        lines.push(line);
        match params::values_for(bench, given, view, name) {
            None => {}
            Some(Ok(paths)) => {
                let several = paths.len() > 1;
                for (path, keys) in paths {
                    let under = if several {
                        format!(" under {path}")
                    } else {
                        String::new()
                    };
                    lines.push(if keys.is_empty() {
                        format!("      no keys{under} on this host")
                    } else {
                        format!("      keys{under}: {}", keys.join(", "))
                    });
                }
            }
            Some(Err(why)) => lines.push(format!("      keys unknown: {why}")),
        }
    }
    output::bench_msg(&lines.join("\n"));
}

fn script_origin(bench: &Bench) -> String {
    match &bench.script {
        BenchScript::Inline { declared_in, .. } => {
            format!("inline script in {}", output::path(declared_in))
        }
        BenchScript::File { path, .. } => format!("script {}", output::path(path)),
    }
}

/// The script as this run executes it, read once: every iteration runs this
/// one compiled program, and the stored config records these bytes.
pub(super) struct Frozen {
    pub(super) bytes: Vec<u8>,
    /// The `.lua` file, or the config file holding the inline script.
    pub(super) origin: PathBuf,
    pub(super) script: lua::Script,
    /// The `bench_lib` modules as read when the run began, which `require`
    /// loads and the stored config records.
    pub(super) library: Option<std::sync::Arc<super::library::Library>>,
}

fn freeze(bench: &Bench) -> Result<Frozen, DevError> {
    let (bytes, origin, chunk, lines) = match &bench.script {
        BenchScript::Inline {
            source,
            declared_in,
        } => (
            source.as_bytes().to_vec(),
            declared_in.clone(),
            None,
            Some(super::InlineLines::of(&bench.name, source, declared_in)),
        ),
        BenchScript::File { path, declared_in } => (
            std::fs::read(path).map_err(|e| {
                DevError::Config(format!(
                    "[bench.{}] script_file {} (set in {}) cannot be read: {e}",
                    bench.name,
                    path.display(),
                    declared_in.display()
                ))
            })?,
            path.clone(),
            Some(path.display().to_string()),
            None,
        ),
    };
    // An inline script is named by its config file, and numbered by that
    // file's lines, when they can be mapped; the map is frozen with it.
    let chunk = match (&chunk, &lines) {
        (Some(chunk), _) => chunk.clone(),
        (None, Some(lines)) => lines.chunk.clone(),
        (None, None) => format!("[bench.{}] script", bench.name),
    };
    let map = lines.as_ref().and_then(|l| l.lines.clone());
    let script = lua::load_script(&bytes, Some(&chunk), map).map_err(|e| {
        let problem = match &lines {
            Some(lines) => lines.problem(&e),
            None => e.to_string(),
        };
        DevError::Config(format!(
            "[bench.{}] {}: {problem}",
            bench.name,
            origin.display()
        ))
    })?;
    if let Some(why) = lines.as_ref().and_then(super::InlineLines::why_not) {
        output::warn(&format!(
            "[bench.{}] script errors count lines from the script string, since {why}",
            bench.name
        ));
    }
    let library = bench
        .library
        .as_deref()
        .map(super::library::Library::read)
        .transpose()?
        .map(std::sync::Arc::new);
    let script = match &library {
        Some(library) => script.with_library(std::sync::Arc::clone(library)),
        None => script,
    };
    Ok(Frozen {
        bytes,
        origin,
        script,
        library,
    })
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Everything one attempt is made of before it runs.
struct Attempt<'a> {
    detection: &'a Detection,
    bench: &'a Bench,
    frozen: &'a Frozen,
    bound: &'a params::Bound,
    /// `bench()`'s arguments, the inputs their handles index, `config()`.
    args: lua::Args,
    inputs: Vec<PathBuf>,
    config: std::sync::Arc<lua::Value>,
    committed: bool,
    hostname: &'a str,
    /// Where the iterations make their directories: under the host's scratch.
    container: &'a Path,
    iterations: u32,
    argv: Vec<String>,
    measurement: store::Measurement,
    /// The library modules this arm's preflight asked for.
    preflight_modules: &'a BTreeSet<String>,
}

impl Attempt<'_> {
    /// One arm's whole attempt: its document started now, under its own
    /// uuid, then the worktree, build and iterations, every outcome decided.
    /// Fails only before the document exists.
    fn run(
        &self,
        commit: &crate::git::Commit,
        tree: &crate::tree_policy::TreePolicy,
        keep: usize,
    ) -> Result<store::RunDocument, DevError> {
        let uuid = crate::db::generate_uuid()?;
        let mut document = self.document(commit, &uuid, unix_ms());
        // Every module the preflight or an iteration asked for, loaded or
        // not, is stored with its source once the attempt is decided.
        let mut modules = self.preflight_modules.clone();
        let worktree = crate::context::with_worktree(
            Some(&self.detection.project),
            &self.detection.project_root,
            &self.detection.build_root,
            // The sha resolved once, so the tree built is the commit recorded
            // even if the ref moves meanwhile.
            self.committed.then_some(commit.as_str()),
            false,
            tree,
            keep,
            |build_root| {
                self.execute(build_root.as_path(), commit, &mut document, &mut modules);
                Ok(())
            },
        );
        if !modules.is_empty() {
            document.config =
                stored_config::render(self.detection, &self.bench.name, self.frozen, &modules);
        }
        if let Err(error) = worktree {
            // A git call cut short by the stop reports a spawn or subprocess
            // error, not an interrupt.
            document.outcome =
                if matches!(error, DevError::Interrupted) || crate::shutdown::should_unwind() {
                    store::RunOutcome::Interrupted
                } else {
                    store::RunOutcome::Failed {
                        reason: format!("worktree: {}", reason_of(&error)),
                    }
                };
        }
        Ok(document)
    }

    /// The document as the attempt starts it: identity, conditions and the
    /// build asked for, its outcome interrupted until something decides it.
    fn document(
        &self,
        commit: &crate::git::Commit,
        uuid: &str,
        started_unix_ms: i64,
    ) -> store::RunDocument {
        let live = self.detection.build_root.as_path();
        let subject = crate::git::commit_subject(live, commit).ok();
        // A `--commit` worktree counts as clean; the live tree is read again
        // once the build is done (`built_as_recorded`).
        let dirty = !self.committed && self.live_dirty();
        let config = &self.detection.config;
        let profile = self.profile();
        let features = build_features(config, self.bench, self.measurement);
        let captured_env = config_env(config);
        store::RunDocument {
            format: store::DOCUMENT_FORMAT,
            uuid: uuid.to_owned(),
            project: self.detection.project.name().to_owned(),
            bench: self.bench.name.clone(),
            checkout: self.detection.build_root.as_path().display().to_string(),
            hostname: self.hostname.to_owned(),
            commit: commit.as_str().to_owned(),
            dirty,
            subject,
            started_unix_ms,
            brokkr_version: env!("CARGO_PKG_VERSION").to_owned(),
            argv: self.argv.clone(),
            conditions: store::Conditions {
                kernel: crate::host::kernel::collect().version,
                cpu_governor: crate::host::cpu::collect().governor,
                avail_memory_mb: i64::try_from(crate::host::memory::collect().available_mb).ok(),
                captured_env,
            },
            build: store::Build {
                target: self.bench.target.clone(),
                profile: profile.to_string(),
                features: features.clone(),
                default_features: self.bench.default_features,
                resolved: None,
                artifact: None,
            },
            measurement: self.measurement,
            params: self.bound.document(),
            config: stored_config::render(
                self.detection,
                &self.bench.name,
                self.frozen,
                &BTreeSet::new(),
            ),
            iterations_requested: self.iterations,
            outcome: store::RunOutcome::Interrupted,
            iterations: Vec::new(),
        }
    }

    /// Build in `build_root` (the live checkout or the `--commit` worktree)
    /// and run, deciding the document's outcome.
    fn execute(
        &self,
        build_root: &Path,
        commit: &crate::git::Commit,
        document: &mut store::RunDocument,
        modules: &mut BTreeSet<String>,
    ) {
        let profile = self.profile();
        let features = document.build.features.clone();
        let program = match self.build(build_root, &profile, &features, document) {
            Ok(program) => program,
            Err(error) => {
                document.outcome =
                    if matches!(error, DevError::Interrupted) || crate::shutdown::should_unwind() {
                        store::RunOutcome::Interrupted
                    } else {
                        store::RunOutcome::Failed {
                            reason: format!("build: {}", reason_of(&error)),
                        }
                    };
                return;
            }
        };
        if !self.built_as_recorded(build_root, commit, document) {
            return;
        }
        self.iterate(&program, commit, document, modules);
    }

    /// Whether the build just made is still what the document records,
    /// observed again once it and its snapshot are done: the tree built must
    /// still be at the recorded commit, else the arm fails with no iteration
    /// run, and the live tree is dirty if it was before the build or is now.
    /// Two observations, so a change present at neither is not seen.
    fn built_as_recorded(
        &self,
        build_root: &Path,
        commit: &crate::git::Commit,
        document: &mut store::RunDocument,
    ) -> bool {
        // No iteration would run; a git read now would be cut short anyway.
        if crate::shutdown::should_unwind() {
            document.outcome = store::RunOutcome::Interrupted;
            return false;
        }
        let head = match crate::git::resolve_commit(build_root, "HEAD") {
            Ok(head) => head,
            Err(error) => {
                document.outcome = if crate::shutdown::should_unwind() {
                    store::RunOutcome::Interrupted
                } else {
                    store::RunOutcome::Failed {
                        reason: format!(
                            "HEAD could not be read after the build: {}",
                            reason_of(&error)
                        ),
                    }
                };
                return false;
            }
        };
        // An observed move fails the arm whatever a stop does meanwhile: the
        // build may have read either tree, so the commit it records is false.
        if head != *commit {
            document.outcome = store::RunOutcome::Failed {
                reason: format!(
                    "HEAD moved from {} to {} during the build, so what was built is not the \
                     commit recorded",
                    commit.as_str(),
                    head.as_str()
                ),
            };
            return false;
        }
        if !self.committed {
            // Read even when already dirty, then joined: once dirty, a run
            // stays dirty, since the build may have read what was there.
            let dirty_after = self.live_dirty();
            document.dirty = document.dirty || dirty_after;
        }
        true
    }

    /// The live tree under the measurement policy, its config files exempt;
    /// a tree git cannot read counts as dirty.
    fn live_dirty(&self) -> bool {
        !crate::git::TreeState::read_for_measurement(
            self.detection.build_root.as_path(),
            &self.detection.config.included_config_files,
        )
        .is_some_and(|state| !state.for_measurement())
    }

    fn profile(&self) -> crate::build::RustProfile {
        self.bench.profile.clone().unwrap_or_else(|| {
            crate::runnables::build_profile(self.detection.config.bin.as_ref(), None)
        })
    }

    /// Resolve and build the target, snapshot it, and fill in the document's
    /// build as far as it got. Returns the snapshot every child runs.
    fn build(
        &self,
        build_root: &Path,
        profile: &crate::build::RustProfile,
        features: &[String],
        document: &mut store::RunDocument,
    ) -> Result<PathBuf, DevError> {
        let config = &self.detection.config;
        let (package, target, example) = crate::runnables::resolve_build_target(
            build_root,
            config.bin.as_ref(),
            self.bench.target.as_deref(),
        )?;
        let build = crate::build::BuildConfig {
            package: Some(package),
            bin: (!example).then(|| target.clone()),
            example: example.then(|| target.clone()),
            features: features.to_vec(),
            default_features: self.bench.default_features,
            all_features: false,
            profile: profile.clone(),
        };
        let built = crate::build::cargo_build(&build, build_root)?;
        document.build.resolved = Some(store::ResolvedBuild {
            bin: built.target.clone(),
            features: built.unit_features.clone(),
        });
        // The timing recipe is the bench's features without its hotpath or
        // alloc ones, and the hotpath recipe without the alloc ones it does
        // not share, judged on what cargo resolved in the tree built: a
        // default or another feature that enables one is caught here.
        let barred: Vec<&String> = match self.measurement {
            store::Measurement::Timing => self
                .bench
                .hotpath_features
                .iter()
                .chain(&self.bench.alloc_features)
                .collect(),
            store::Measurement::Hotpath => self
                .bench
                .alloc_features
                .iter()
                .filter(|f| !self.bench.hotpath_features.contains(f))
                .collect(),
            store::Measurement::Alloc => Vec::new(),
        };
        if let Some(on) = barred.into_iter().find(|f| built.unit_features.contains(f)) {
            let (build, fix) = match self.measurement {
                store::Measurement::Hotpath => ("hotpath", "with --alloc"),
                _ => ("timing", "with --hotpath or --alloc"),
            };
            return Err(DevError::Refused(format!(
                "the {build} build resolved profiling feature {on:?}, which instruments it: run \
                 {fix}, or keep {on:?} out of the bench's features and their defaults"
            )));
        }
        let artifacts =
            crate::clean::layout::ProjectState::new(self.detection.project_root.as_path())
                .artifacts_dir();
        let (program, digest) = crate::runnables::snapshot_executable(&built.path, &artifacts)?;
        document.build.artifact = Some(digest);
        Ok(program)
    }

    /// Every iteration, stopping at the first that does not complete, and
    /// the run's outcome from them.
    fn iterate(
        &self,
        program: &Path,
        commit: &crate::git::Commit,
        document: &mut store::RunDocument,
        modules: &mut BTreeSet<String>,
    ) {
        let state = crate::clean::layout::ProjectState::new(self.detection.project_root.as_path());
        let mut since = host::Since::new(self.detection.build_root.as_path(), commit.clone());
        let mut report_budget = host::RUN_REPORT_BYTES;
        let total = self.iterations;
        for index in 0..total {
            if crate::shutdown::should_unwind() {
                document.outcome = store::RunOutcome::Interrupted;
                return;
            }
            let (iteration, returned, budget_left, required) =
                host::run_iteration(host::IterationSetup {
                    script: &self.frozen.script,
                    args: &self.args,
                    inputs: &self.inputs,
                    config: std::sync::Arc::clone(&self.config),
                    program,
                    cwd: self.detection.project_root.as_path(),
                    state_root: state.root(),
                    container: self.container,
                    index: usize::try_from(index).unwrap_or(usize::MAX),
                    since,
                    measurement: self.measurement,
                    report_budget,
                });
            since = returned;
            report_budget = budget_left;
            modules.extend(required);
            let label = format!("iteration {}/{total}", index + 1);
            let outcome = iteration.outcome.clone();
            match (&outcome, iteration.wall_us) {
                (store::IterationOutcome::Completed { verdict }, Some(wall)) => {
                    let wall = std::time::Duration::from_micros(u64::try_from(wall).unwrap_or(0));
                    output::bench_msg(&format!(
                        "{label}: {}{}",
                        output::format_duration(wall),
                        verdict
                            .as_ref()
                            .map(|v| format!(", verdict {v:?}"))
                            .unwrap_or_default()
                    ));
                }
                (store::IterationOutcome::Failed { reason }, _) => {
                    output::warn(&format!("{label} failed: {reason}"));
                }
                _ => output::warn(&format!("{label} interrupted")),
            }
            document.iterations.push(iteration);
            match outcome {
                store::IterationOutcome::Completed { .. } => {}
                store::IterationOutcome::Failed { reason } => {
                    document.outcome = store::RunOutcome::Failed {
                        reason: format!("{label}: {reason}"),
                    };
                    return;
                }
                store::IterationOutcome::Interrupted => {
                    document.outcome = store::RunOutcome::Interrupted;
                    return;
                }
            }
        }
        let verdict_of = |i: &store::Iteration| match &i.outcome {
            store::IterationOutcome::Completed { verdict } => verdict.clone(),
            _ => None,
        };
        let first = document.iterations.first().and_then(verdict_of);
        document.outcome = if document.iterations.iter().all(|i| verdict_of(i) == first) {
            store::RunOutcome::Completed
        } else {
            let seen: Vec<String> = document
                .iterations
                .iter()
                .map(|i| verdict_of(i).map_or_else(|| "nil".to_owned(), |v| format!("{v:?}")))
                .collect();
            store::RunOutcome::Failed {
                reason: format!(
                    "the iterations disagree on the verdict: {}",
                    seen.join(", ")
                ),
            }
        };
    }
}

/// `capture_env` as the document stores it: credential values redacted.
fn config_env(config: &crate::config::DevConfig) -> BTreeMap<String, String> {
    super::evidence::redacted_env(&crate::config::captured_env_map(config))
}

/// An error's text for a document's failure reason, never empty.
fn reason_of(error: &DevError) -> String {
    let shown = error.to_string();
    if shown.trim().is_empty() {
        format!("{error:?}")
    } else {
        shown
    }
}

/// Where a run is published: the local bench store, or the results server
/// the user-wide config names.
enum Destination {
    Local { store: store::Store, path: PathBuf },
    Remote(super::remote::Client),
}

impl Destination {
    fn open(config: &crate::config::DevConfig) -> Result<Self, DevError> {
        if let Some(server) = &config.results_server {
            let client = super::remote::Client::new(server);
            client.probe()?;
            return Ok(Self::Remote(client));
        }
        let path = super::evidence::store_path()?;
        Ok(Self::Local {
            store: super::evidence::open_store(&path)?,
            path,
        })
    }

    fn shown(&self) -> String {
        match self {
            Self::Local { path, .. } => output::path(path).to_string(),
            Self::Remote(client) => format!("results server {}", client.shown()),
        }
    }

    /// Publish `document`, latching uncertainty: once an attempt's outcome
    /// is unknown, only a success settles it (`super::publication`).
    fn publish(&mut self, document: &store::RunDocument) -> Result<(), DevError> {
        match self {
            Self::Local { store, path } => {
                let place = path.display().to_string();
                super::publication::publish(
                    &document.uuid,
                    &place,
                    super::publication::Policy::LOCAL,
                    || {
                        let published = store.publish(document);
                        super::publication::local_attempt(published.as_ref().map(drop))
                    },
                )
            }
            Self::Remote(client) => client.publish(document),
        }
    }
}

/// The run's one result line, never quiet-suppressed: its uuid is the only
/// handle `brokkr results` finds it by.
fn report(document: &store::RunDocument, shown: &str, place: &str, started: Instant) {
    let outcome = match &document.outcome {
        store::RunOutcome::Completed => {
            let best = document
                .best_iteration()
                .and_then(|i| document.iterations[i].wall_us)
                .map_or_else(String::new, |us| {
                    format!(
                        ", best {} of {}",
                        output::format_duration(std::time::Duration::from_micros(
                            u64::try_from(us).unwrap_or(0)
                        )),
                        document.iterations.len()
                    )
                });
            format!("completed{best}")
        }
        store::RunOutcome::Failed { .. } => "failed".to_owned(),
        store::RunOutcome::Interrupted => "interrupted".to_owned(),
    };
    output::result_forced(&format!(
        "bench {shown}: run {} {outcome}{} in {} ({}); `brokkr results {}` shows it",
        document.uuid,
        if document.dirty { ", dirty tree" } else { "" },
        output::format_duration(started.elapsed()),
        place,
        crate::db::short_uuid(&document.uuid),
    ));
}
