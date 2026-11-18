//! `brokkr bench --rounds N`: an A/B invocation. Its competitors differ on
//! exactly one axis, a param's values (`--versus`), the commit built
//! (`--commit` more than once) or the bench (NAME a comma list); whatever
//! else fans out is the workload, and the competitors of one workload are
//! matched by its key (`plan`). Each workload runs `N` rounds of its
//! competitors back to back, the order reversed in every other round, so
//! over two rounds each competitor takes the same mean position and each
//! pair runs once in each relative order. Nothing is claimed of the
//! transitions across a round's or a workload's boundary.
//!
//! Everything before the first build is the invocation-wide preparation the
//! plain runner makes: bind, validate, hold the union of inputs, preflight
//! (per bench and commit, each commit's `since()` answers kept for its
//! cells), digest. Then every distinct build is made once, up front, and
//! every cell of it runs its snapshot, so no cell pays for or follows a build
//! and a ref or HEAD that moves later cannot make a cell's commit false. A
//! build that fails is stored once, as a build-phase run attributed to the
//! first slot it serves; the slots it serves are blocked and never run.
//! Every cell is its own run carrying the whole plan (`store::Schedule`), so
//! one stored run names every slot planned.

mod plan;

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::Instant;

use super::{
    Attempt, BenchArgs, Destination, find, freeze, hold_inputs, host, iteration_container,
    measurement_of, not_run, preflight_arms, publish_arm, unix_ms, validate,
};
use crate::bench::{lua, params, store, view};
use crate::error::DevError;
use crate::output;
use crate::project::Detection;
use plan::{Entry, Grid, Recipe};

/// What makes two builds one: the tree, everything cargo is asked, and the
/// profiling features the bench bars from what cargo resolves, which the
/// build checks for the bench it builds for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BuildKey {
    commit: usize,
    target: Option<String>,
    profile: String,
    features: Vec<String>,
    default_features: bool,
    hotpath_features: Vec<String>,
    alloc_features: Vec<String>,
}

/// A build made once, which every cell of it runs.
struct Built {
    program: PathBuf,
    build: store::Build,
    dirty: bool,
}

/// The invocation, planned and validated, before anything is held or run.
struct Planned<'a> {
    names: &'a str,
    entries: Vec<Entry<'a>>,
    config: std::sync::Arc<lua::Value>,
    hostname: String,
    measurement: store::Measurement,
    /// No `--commit`: the one build is the live tree's.
    live: bool,
    commits: Vec<crate::git::Commit>,
    grid: Grid,
    builds: Vec<BuildKey>,
    plan: store::Plan,
    iterations: u32,
}

pub(super) fn run(
    args: &BenchArgs,
    names: &str,
    detection: &Detection,
    invocation: &crate::db::RunInvocation,
) -> Result<(), DevError> {
    let mut planned = plan_invocation(args, names, detection)?;
    if args.dry_run {
        return dry_run(&mut planned, detection);
    }
    execute(planned, detection, invocation)
}

/// Every bench named, each once, and the one axis the competitors are on.
fn benches_of<'a>(
    args: &BenchArgs,
    names: &str,
    detection: &'a Detection,
) -> Result<Vec<&'a crate::config::bench::Bench>, DevError> {
    if args.rounds.is_some_and(|n| n.get() > store::MAX_ROUNDS) {
        return Err(DevError::Usage(format!(
            "--rounds takes at most {}",
            store::MAX_ROUNDS
        )));
    }
    let mut seen = BTreeSet::new();
    let mut benches = Vec::new();
    for name in names.split(',') {
        if name.is_empty() {
            return Err(DevError::Usage(format!(
                "bench {names}: an empty name in the list"
            )));
        }
        if !seen.insert(name) {
            return Err(DevError::Usage(format!(
                "bench {names}: {name} is listed twice"
            )));
        }
        benches.push(find(&detection.config.benches, name)?);
    }
    let axes = [
        benches.len() > 1,
        args.commits.len() > 1,
        args.versus.is_some(),
    ];
    match axes.iter().filter(|a| **a).count() {
        1 => Ok(benches),
        0 => Err(DevError::Usage(
            "--rounds needs competitors on one axis: --versus PARAM with that param's values, \
             --commit given more than once, or NAME a comma list of benches"
                .into(),
        )),
        _ => Err(DevError::Usage(
            "--rounds takes competitors on one axis only: --versus, several --commit or a bench \
             list, not more than one of them"
                .into(),
        )),
    }
}

/// Every ref resolved once, before anything runs, with its label: the sha
/// is what each build checks out and records. Two refs naming one commit
/// are refused, since the competitors would not differ.
fn resolve_commits(
    args: &BenchArgs,
    detection: &Detection,
) -> Result<(Vec<crate::git::Commit>, Vec<String>), DevError> {
    let refs: Vec<&str> = if args.commits.is_empty() {
        vec!["HEAD"]
    } else {
        args.commits.iter().map(String::as_str).collect()
    };
    let mut commits: Vec<crate::git::Commit> = Vec::with_capacity(refs.len());
    for reference in &refs {
        let commit = crate::git::resolve_commit(detection.build_root.as_path(), reference)?;
        if let Some(at) = commits.iter().position(|c| *c == commit) {
            return Err(DevError::Usage(format!(
                "--commit {} and --commit {reference} are one commit, {}: competitors must \
                 differ",
                refs[at],
                commit.as_str()
            )));
        }
        commits.push(commit);
    }
    let labels = refs
        .iter()
        .zip(&commits)
        .map(|(reference, commit)| {
            let short = crate::db::short_uuid(commit.as_str());
            if commit.as_str().starts_with(&reference.to_ascii_lowercase()) {
                short.clone()
            } else {
                format!("{short} ({reference})")
            }
        })
        .collect();
    Ok((commits, labels))
}

/// Bind, validate and plan the invocation: refused whole, nothing stored,
/// when any of it fails.
fn plan_invocation<'a>(
    args: &BenchArgs,
    names: &'a str,
    detection: &'a Detection,
) -> Result<Planned<'a>, DevError> {
    let benches = benches_of(args, names, detection)?;
    let mut measurement = None;
    for bench in &benches {
        let this = measurement_of(args, bench)?;
        measurement.get_or_insert(this);
    }
    let measurement = measurement.unwrap_or(store::Measurement::Timing);
    let hostname = crate::config::hostname()?;
    let view = view::build(
        detection.config_sources.composed(),
        &hostname,
        detection.project_root.as_path(),
    );
    let mut entries = Vec::with_capacity(benches.len());
    let mut config = None;
    let listed = benches.len() > 1;
    // A bench list binds one set of params in every bench: one a bench does
    // not take, or a required one it lacks, refuses the list.
    let shared = |error: DevError| match error {
        DevError::Usage(why) if listed => DevError::Usage(format!(
            "{why}; the params after a bench list are bound by every bench in it, so a param \
             only some take cannot be given"
        )),
        DevError::Config(why) if listed => DevError::Config(format!(
            "{why}; the params after a bench list are bound by every bench in it"
        )),
        other => other,
    };
    for bench in benches {
        let given = params::parse(bench, &args.params).map_err(shared)?;
        if given.help {
            return Err(DevError::Usage(format!(
                "bench {}: --help describes a bench without --rounds",
                bench.name
            )));
        }
        let arms = params::bind(bench, &given, &view).map_err(shared)?;
        let frozen = freeze(bench)?;
        let (this, labels, validated) = validate(&bench.name, bench, &view, &frozen, &arms)?;
        config.get_or_insert(this);
        entries.push(Entry {
            bench,
            frozen,
            arms,
            labels,
            given,
            validated,
        });
    }
    let config = config.ok_or_else(|| DevError::Internal("--rounds bound no bench".into()))?;
    let (commits, commit_labels) = resolve_commits(args, detection)?;
    let grid = match (&args.versus, args.commits.len() > 1) {
        (Some(name), _) => plan::by_param(&entries[0], name, &entries[0].bench.name)?,
        (None, true) => plan::by_build(&entries[0], &commit_labels)?,
        (None, false) => plan::by_bench(&entries)?,
    };
    // One build per distinct tree and cargo request, in the order the
    // competitors first need them.
    let mut builds: Vec<BuildKey> = Vec::new();
    let mut build_of: Vec<u32> = Vec::with_capacity(grid.competitors.len());
    for recipe in &grid.recipes[0] {
        let bench = entries[recipe.bench].bench;
        let key = BuildKey {
            commit: recipe.commit,
            target: bench.target.clone(),
            profile: super::profile_of(detection, bench).to_string(),
            features: super::build_features(&detection.config, bench, measurement),
            default_features: bench.default_features,
            hotpath_features: bench.hotpath_features.clone(),
            alloc_features: bench.alloc_features.clone(),
        };
        build_of.push(u32::try_from(plan::intern(&mut builds, key)).unwrap_or(u32::MAX));
    }
    let plan = store::Plan {
        axis: grid.axis.clone(),
        competitors: grid.competitors.clone(),
        workloads: grid.workloads.clone(),
        rounds: args.rounds.map_or(1, NonZeroU32::get),
        builds: build_of,
    };
    within_slots(&plan)?;
    Ok(Planned {
        names,
        entries,
        config,
        hostname,
        measurement,
        live: args.commits.is_empty(),
        commits,
        grid,
        builds,
        plan,
        // A round is the repetition: a cell runs once unless asked.
        iterations: args.iterations.map_or(1, NonZeroU32::get),
    })
}

/// Refused past the cells one invocation may plan, which the store refuses
/// too.
fn within_slots(plan: &store::Plan) -> Result<(), DevError> {
    if plan.slots().is_none_or(|n| n > store::MAX_SLOTS) {
        return Err(DevError::Usage(format!(
            "--rounds: {} workloads, {} competitors and {} rounds make more than the {} cells an \
             invocation may plan",
            plan.workloads.len(),
            plan.competitors.len(),
            plan.rounds,
            store::MAX_SLOTS
        )));
    }
    Ok(())
}

/// `--dry-run`: the plan printed, every bound file's shape and pin checked,
/// nothing held, preflighted, built or stored.
fn dry_run(planned: &mut Planned<'_>, detection: &Detection) -> Result<(), DevError> {
    for entry in &mut planned.entries {
        params::digest_arms(
            &mut entry.arms,
            params::Depth::PinsOnly,
            detection.project_root.as_path(),
            &detection.config_sources,
        )?;
    }
    let (grid, plan) = (&planned.grid, &planned.plan);
    output::dry_run_msg(&format!(
        "bench {}: {} rounds of {} competitors by {} in {}, {} each, {}",
        planned.names,
        plan.rounds,
        grid.competitors.len(),
        grid.axis.name(),
        output::count(grid.workloads.len(), "workload"),
        output::count(
            usize::try_from(planned.iterations).unwrap_or(usize::MAX),
            "iteration"
        ),
        output::count(planned.builds.len(), "build")
    ));
    for (c, competitor) in grid.competitors.iter().enumerate() {
        output::dry_run_msg(&format!(
            "competitor {}/{}: {competitor}, build {}",
            c + 1,
            grid.competitors.len(),
            plan.builds[c] + 1
        ));
    }
    for (w, workload) in grid.workloads.iter().enumerate() {
        if !workload.is_empty() {
            output::dry_run_msg(&format!(
                "workload {}/{}: {workload}",
                w + 1,
                grid.workloads.len()
            ));
        }
    }
    for (index, build) in planned.builds.iter().enumerate() {
        output::dry_run_msg(&format!(
            "build {}/{}: {} ({}, features [{}]) at {}{}",
            index + 1,
            planned.builds.len(),
            build.target.as_deref().unwrap_or("[bin] default"),
            build.profile,
            build.features.join(", "),
            crate::db::short_uuid(planned.commits[build.commit].as_str()),
            if planned.live { "" } else { " in a worktree" }
        ));
    }
    output::dry_run_msg("ok");
    Ok(())
}

/// Each bench's preflight for each commit its cells run, its modules joined
/// to the arm's `validate()` ones, every commit's `since()` answers kept in
/// `sinces`; then every bound file digested.
fn preflight_and_digest(
    planned: &mut Planned<'_>,
    detection: &Detection,
    sinces: &mut [host::Since],
) -> Result<Modules, DevError> {
    crate::serve::refuse_measurement("a script bench")?;
    let needed: BTreeSet<(usize, usize)> = planned
        .grid
        .recipes
        .iter()
        .flatten()
        .map(|r| (r.bench, r.commit))
        .collect();
    let mut modules = BTreeMap::new();
    let guard = crate::shutdown::SigtermGuard::install();
    let ran = (|| {
        for &(b, k) in &needed {
            let entry = &planned.entries[b];
            let found = preflight_arms(
                &entry.bench.name,
                &entry.frozen,
                &entry.arms,
                &entry.labels,
                &planned.config,
                detection,
                &mut sinces[k],
            )?;
            let joined: Vec<BTreeSet<String>> = found
                .into_iter()
                .zip(entry.validated.iter().cloned())
                .map(|(mut preflight, validate)| {
                    preflight.extend(validate);
                    preflight
                })
                .collect();
            modules.insert((b, k), joined);
        }
        Ok::<(), DevError>(())
    })();
    if guard.leave() {
        return Err(DevError::Interrupted);
    }
    ran?;
    for entry in &mut planned.entries {
        params::digest_arms(
            &mut entry.arms,
            params::Depth::Every,
            detection.project_root.as_path(),
            &detection.config_sources,
        )?;
    }
    Ok(modules)
}

/// Each (bench, commit)'s arms' library modules, preflight and validate.
type Modules = BTreeMap<(usize, usize), Vec<BTreeSet<String>>>;

/// What every cell and build shares once preparation is done.
struct Run<'p, 'a> {
    planned: &'p Planned<'a>,
    detection: &'p Detection,
    container: PathBuf,
    argv: Vec<String>,
    modules: Modules,
    invocation: String,
}

impl Run<'_, '_> {
    fn attempt(&self, recipe: &Recipe) -> Attempt<'_> {
        let entry = &self.planned.entries[recipe.bench];
        let bound = &entry.arms[recipe.arm];
        Attempt {
            detection: self.detection,
            bench: entry.bench,
            frozen: &entry.frozen,
            bound,
            args: bound.args(),
            inputs: bound.inputs(),
            config: std::sync::Arc::clone(&self.planned.config),
            committed: !self.planned.live,
            hostname: &self.planned.hostname,
            container: &self.container,
            iterations: self.planned.iterations,
            argv: self.argv.clone(),
            measurement: self.planned.measurement,
            preflight_modules: &self.modules[&(recipe.bench, recipe.commit)][recipe.arm],
        }
    }

    fn schedule(&self, slot: store::Slot, phase: store::Phase) -> store::Schedule {
        store::Schedule {
            invocation: self.invocation.clone(),
            plan: self.planned.plan.clone(),
            slot,
            phase,
        }
    }

    fn slot(&self, workload: usize, competitor: usize, round: u32) -> store::Slot {
        let count = u32::try_from(self.planned.grid.competitors.len()).unwrap_or(u32::MAX);
        let competitor = u32::try_from(competitor).unwrap_or(u32::MAX);
        store::Slot {
            workload: u32::try_from(workload).unwrap_or(u32::MAX),
            competitor,
            round,
            position: store::position(competitor, count, round),
        }
    }

    fn bench_name(&self, recipe: &Recipe) -> &str {
        &self.planned.entries[recipe.bench].bench.name
    }
}

fn execute(
    mut planned: Planned<'_>,
    detection: &Detection,
    invocation: &crate::db::RunInvocation,
) -> Result<(), DevError> {
    // Opened, or probed, before anything is built or run.
    let container = iteration_container(detection, &planned.hostname)?;
    let mut destination = Destination::open(&detection.config)?;
    let every_arm: Vec<params::Bound> = planned
        .entries
        .iter()
        .flat_map(|e| e.arms.clone())
        .collect();
    let _inputs_held = hold_inputs(&every_arm)?;
    let keep = crate::context::worktree_keep(&detection.config)?;
    let tree = crate::tree_policy::TreePolicy::of(&detection.config);
    let mut sinces: Vec<host::Since> = planned
        .commits
        .iter()
        .map(|commit| host::Since::new(detection.build_root.as_path(), commit.clone()))
        .collect();
    let modules = preflight_and_digest(&mut planned, detection, &mut sinces)?;
    // Worktree setup, builds, iterations and publication: a stop is
    // recorded, so the run in flight is still published.
    let _interrupts = crate::shutdown::SigtermGuard::install();
    let run = Run {
        planned: &planned,
        detection,
        container,
        argv: crate::history::redact_secret_argv(&invocation.argv),
        modules,
        invocation: crate::db::generate_uuid()?,
    };
    let mut failed: Vec<String> = Vec::new();
    let made = build_all(&run, &tree, keep, &mut destination, &mut failed)?;
    run_cells(&run, &made, &mut sinces, &mut destination, &mut failed)?;
    let id = crate::db::short_uuid(&run.invocation);
    output::bench_msg(&format!(
        "invocation {id}: `brokkr results --invocation {id}` summarizes it"
    ));
    if failed.is_empty() {
        return Ok(());
    }
    Err(DevError::Reported(format!(
        "bench {}: {} failed: {}",
        planned.names,
        failed.len(),
        failed.join("; ")
    )))
}

/// Every build, once each: the snapshot each made, `None` for one that
/// failed, which is stored as its own build-phase run.
fn build_all(
    run: &Run<'_, '_>,
    tree: &crate::tree_policy::TreePolicy,
    keep: usize,
    destination: &mut Destination,
    failed: &mut Vec<String>,
) -> Result<Vec<Option<Built>>, DevError> {
    let planned = run.planned;
    let mut made = Vec::with_capacity(planned.builds.len());
    for (index, build) in planned.builds.iter().enumerate() {
        let competitor = planned
            .plan
            .builds
            .iter()
            .position(|b| *b as usize == index)
            .unwrap_or(0);
        let recipe = planned.grid.recipes[0][competitor];
        let commit = &planned.commits[build.commit];
        // A live build is of HEAD as resolved for the invocation: once a
        // preflight or an earlier build may have moved it, it is read again
        // before this build starts, as after it.
        if let Err(error) = live_head_holds(planned, run.detection, commit) {
            not_run(planned.names, &["every cell".to_owned()]);
            return Err(error);
        }
        output::bench_msg(&format!(
            "build {}/{}: {} for {}",
            index + 1,
            planned.builds.len(),
            run.bench_name(&recipe),
            planned.grid.competitors[competitor]
        ));
        let attempt = run.attempt(&recipe);
        let uuid = crate::db::generate_uuid()?;
        let started = Instant::now();
        let mut document = attempt.document(commit, &uuid, unix_ms());
        if let Some(program) = attempt.build_into(commit, tree, keep, &mut document) {
            made.push(Some(Built {
                program,
                build: document.build,
                dirty: document.dirty,
            }));
            continue;
        }
        // Stored once, as the build it was: every slot of it is blocked.
        document.schedule = Some(run.schedule(
            run.slot(0, competitor, 0),
            store::Phase::Build {
                build: u32::try_from(index).unwrap_or(u32::MAX),
            },
        ));
        attempt.settle_config(&mut document, attempt.preflight_modules);
        let shown = format!("{} (build {})", run.bench_name(&recipe), index + 1);
        if let Err(error) = publish_arm(destination, &document, &shown, started) {
            not_run(planned.names, &["every cell".to_owned()]);
            return Err(error);
        }
        failed.push(format!("build {}", index + 1));
        made.push(None);
    }
    Ok(made)
}

/// Whether a live build may start: not once a stop is pending, and only
/// while HEAD is still `commit`. A `--commit` build checks out its sha.
fn live_head_holds(
    planned: &Planned<'_>,
    detection: &Detection,
    commit: &crate::git::Commit,
) -> Result<(), DevError> {
    if crate::shutdown::should_unwind() {
        return Err(DevError::Interrupted);
    }
    if !planned.live {
        return Ok(());
    }
    let head = crate::git::resolve_commit(detection.build_root.as_path(), "HEAD")?;
    if head != *commit {
        return Err(DevError::Refused(format!(
            "bench {}: HEAD moved from {} to {} before the build, so it would be recorded \
             under a commit it did not build",
            planned.names,
            commit.as_str(),
            head.as_str()
        )));
    }
    Ok(())
}

/// Every cell, workload by workload, round by round, each round's order
/// reversed from the last; a blocked slot is skipped, a failed cell does not
/// stop the rest, a stop or a publication failure does.
fn run_cells(
    run: &Run<'_, '_>,
    made: &[Option<Built>],
    sinces: &mut [host::Since],
    destination: &mut Destination,
    failed: &mut Vec<String>,
) -> Result<(), DevError> {
    let planned = run.planned;
    if made.iter().all(Option::is_none) {
        return Ok(());
    }
    let grid = &planned.grid;
    let rounds = planned.plan.rounds;
    let count = u32::try_from(grid.competitors.len()).unwrap_or(u32::MAX);
    let total = grid.workloads.len()
        * usize::try_from(rounds).unwrap_or(usize::MAX)
        * grid.competitors.len();
    let mut done = 0usize;
    for (w, row) in grid.recipes.iter().enumerate() {
        for round in 0..rounds {
            for position in 0..count {
                // The position map is its own inverse.
                let c = store::position(position, count, round) as usize;
                let recipe = row[c];
                done += 1;
                let Some(built) = made[planned.plan.builds[c] as usize].as_ref() else {
                    continue;
                };
                let label = cell_label(grid, w, c, round, rounds);
                if crate::shutdown::should_unwind() {
                    not_run(
                        planned.names,
                        &[format!("{} cells from {label}", total - done + 1)],
                    );
                    return Err(DevError::Interrupted);
                }
                output::bench_msg(&format!("cell {done}/{total}: {label}"));
                let started = Instant::now();
                let document = cell(run, &recipe, built, run.slot(w, c, round), sinces)?;
                let shown = format!("{} ({label})", run.bench_name(&recipe));
                if let Err(error) = publish_arm(destination, &document, &shown, started) {
                    if done < total {
                        not_run(
                            planned.names,
                            &[format!("{} cells after {label}", total - done)],
                        );
                    }
                    return Err(error);
                }
                if matches!(document.outcome, store::RunOutcome::Failed { .. }) {
                    failed.push(label);
                }
            }
        }
    }
    Ok(())
}

/// How a cell is named: its competitor, its workload when there are
/// several, its round.
fn cell_label(grid: &Grid, w: usize, c: usize, round: u32, rounds: u32) -> String {
    let workload = &grid.workloads[w];
    format!(
        "{}{}, round {}/{rounds}",
        grid.competitors[c],
        if workload.is_empty() {
            String::new()
        } else {
            format!(" [{workload}]")
        },
        round + 1
    )
}

/// One cell's run: its document started now under its own uuid, the build
/// it runs recorded as built, its iterations against that snapshot.
fn cell(
    run: &Run<'_, '_>,
    recipe: &Recipe,
    built: &Built,
    slot: store::Slot,
    sinces: &mut [host::Since],
) -> Result<store::RunDocument, DevError> {
    let attempt = run.attempt(recipe);
    let commit = &run.planned.commits[recipe.commit];
    let uuid = crate::db::generate_uuid()?;
    let mut document = attempt.document(commit, &uuid, unix_ms());
    document.build = built.build.clone();
    document.dirty = built.dirty;
    document.schedule = Some(run.schedule(slot, store::Phase::Measure));
    let mut asked = attempt.preflight_modules.clone();
    let since = &mut sinces[recipe.commit];
    let lent = std::mem::replace(since, since.blank());
    *since = attempt.iterate(&built.program, &mut document, &mut asked, lent);
    attempt.settle_config(&mut document, &asked);
    Ok(document)
}
