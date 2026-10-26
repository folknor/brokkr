//! `brokkr bench` - the criterion runner.
//!
//! Not project-gated: any Rust repo with criterion bench targets, the same
//! posture as `run`, `deps` and `clippy`.
//!
//! ## Measurement and comparison are different verbs
//!
//! `cargo bench` with no baseline flags writes to a baseline literally named
//! `base` and compares against whatever `base` held before, so each run silently
//! destroys the previous reference. That is fine for a tight edit loop and
//! useless for studying three commits: the second measurement eats the first.
//!
//! So a measuring run always saves under a name derived from the commit, and
//! comparison is a separate invocation that samples nothing:
//! `--load-baseline B --baseline A` makes criterion load stored data as the new
//! side instead of measuring it. The expensive thing therefore happens once per
//! commit, and you can ask as many questions of the result as you like. It still
//! builds and launches the bench binary - "no sampling" is cheap, not instant.
//!
//! ## Where baselines live
//!
//! Under `.brokkr/bench/`, via criterion's `CRITERION_HOME`, rather than the
//! default `target/criterion`. Baselines are results, not build artifacts: they
//! cost minutes each to produce and are not reconstructible from the source
//! tree, so they must not sit in a directory whose entire purpose is being safe
//! to delete. Putting them in a brokkr-designated directory also means `clean`
//! spares them by the existing rule rather than by a special case, and a user's
//! own `cargo clean` cannot reach them at all.
//!
//! ## The invocation brokkr constructs
//!
//! `-p` comes from the bench target's owning package (see [`discover`]),
//! `--no-fail-fast` and the bench profile are constants, and `CRITERION_HOME`
//! is set. Everything after `--` is forwarded untouched, which is where
//! `--sample-size` / `--measurement-time` / `--noise-threshold` go: those are
//! precisely the knobs a near-noise-floor effect needs, and fixing them in
//! brokkr would defeat the reason for having them.

mod discover;
mod stamp;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::context::{acquire_cmd_lock_opt, with_worktree};
use crate::error::DevError;
use crate::output;

pub use discover::BenchTarget;

/// Parsed `brokkr bench` invocation.
pub struct BenchArgs {
    /// Bench target name; `None` is the bare index.
    pub target: Option<String>,
    /// Measure this commit in a persistent worktree instead of the live tree.
    pub commit: Option<String>,
    /// Compare two already-stored baselines, sampling nothing.
    pub compare: Option<(String, String)>,
    /// List the baselines brokkr has recorded.
    pub baselines: bool,
    /// Explicit baseline name, required when saving from a dirty tree.
    pub name: Option<String>,
    /// Downgrade an environment mismatch from a refusal to a warning.
    pub lenient: bool,
    /// Verbatim passthrough to the criterion harness.
    pub args: Vec<String>,
}

/// Entry point. Startup detection is optional because `bench` also works in a
/// tree with no `brokkr.toml`; when present, it supplies the already-loaded
/// config and source provenance used by tree-state policy.
pub fn run(
    args: &BenchArgs,
    detection: Option<&crate::project::Detection>,
) -> Result<(), DevError> {
    // The two roots are distinct and both matter here. `project_root` holds
    // `brokkr.toml` and anchors everything brokkr owns, including `.brokkr/`;
    // `build_root` is the working directory where cargo and git run. They
    // differ when the config sits one level *above* cwd, which is the layout
    // for driving a checkout that isn't ours - and in that layout the whole
    // point is that brokkr's state stays in the parent and the foreign repo
    // stays clean. Anchoring the baseline store to the build root would write
    // into that repo and, worse, dirty the very tree whose cleanliness decides
    // whether a baseline may be saved.
    let fallback_roots = detection
        .is_none()
        .then(crate::project::detect_roots)
        .transpose()?;
    let (project, project_root, build_root, tree, keep, included_config_files) =
        match (detection, fallback_roots.as_ref()) {
            (Some(d), _) => {
                let keep = d.config.worktree_keep(&crate::config::hostname()?);
                (
                    Some(d.project.clone()),
                    &d.project_root,
                    &d.build_root,
                    crate::tree_policy::TreePolicy::of(&d.config),
                    keep,
                    d.config_sources.included().to_vec(),
                )
            }
            (None, Some((project_root, build_root))) => (
                None,
                project_root,
                build_root,
                crate::tree_policy::TreePolicy::default(),
                crate::worktree_record::DEFAULT_KEEP,
                Vec::new(),
            ),
            (None, None) => {
                return Err(DevError::Internal(
                    "bench roots were not resolved for an unconfigured tree".into(),
                ));
            }
        };
    // `.brokkr/bench` under the project root - the criterion home and the
    // stamp store both live here. The project root (the directory holding
    // `brokkr.toml`) anchors every other `.brokkr/` store, so baselines
    // measured at different commits land in one store rather than following
    // each `--commit` worktree, and a config one level above a foreign
    // checkout keeps them out of a repo that isn't ours.
    let home = project_root.bench_home();

    // Listing what we recorded needs neither cargo nor the lock.
    if args.baselines {
        return list_baselines(&home);
    }

    let compare = args.compare.clone();

    // Resolve the baseline name HERE, before the lock and therefore before
    // brokkr has touched the tree. Taking the lock activates the tree policy,
    // which renames `rust-toolchain.toml` to a `.brokkr-disabled` sidecar (a
    // deletion plus an untracked file) and may put a filtered cargo config in
    // place - exactly what `git status --porcelain` reports as a dirty tree.
    // Asking afterwards means the guard reads brokkr's own side effect as the
    // user's uncommitted work and refuses every run in such a project.
    //
    // The guard exists to catch changes that make a commit hash a lie about
    // what was measured. It therefore has to be asked at a moment when the only
    // thing that could have made the tree dirty is the user.
    let baseline = match (&compare, args.target.as_deref()) {
        // Comparison samples nothing, and the bare index measures nothing;
        // neither needs a name, and neither should be blocked by a dirty tree.
        (Some(_), _) | (_, None) => None,
        (None, Some(_)) => Some(resolve_baseline_name(
            build_root.as_path(),
            &included_config_files,
            args,
        )?),
    };

    // Under `--commit`, `with_worktree` holds the lock and moves the
    // worktree's pin aside itself, so the lock taken *inside* re-enters that
    // hold; without it, the lock taken inside is the fresh hold that moves the
    // live root's pin. Hence lock-inside-closure, not outside.
    // `with_worktree` cuts its worktree from the build root, so the tree
    // checked out is the code tree rather than the config directory above it.
    // Worktree, toolchain and target-dir narration is run-log only.
    let _log =
        crate::run_log::RunLog::begin(project_root.as_path(), crate::run_log::run_log_kind::BENCH);
    with_worktree(
        project.as_ref(),
        project_root,
        build_root,
        args.commit.as_deref(),
        false,
        &tree,
        keep,
        |wt| {
            // The lock context's state root is the *project* root, what
            // `brokkr lock` displays; its build root is the tree this closure
            // builds in, whose `target` the disk gate probes.
            let _lock = acquire_cmd_lock_opt(project.as_ref(), project_root, wt)?;
            let build_root = wt.as_path();

            let targets = discover::discover(build_root)?;
            if targets.is_empty() {
                return Err(DevError::Refused(
                    "no bench targets in this workspace - nothing to benchmark".into(),
                ));
            }
            let Some(wanted) = args.target.as_deref() else {
                output::run_msg(discover::index(&targets).trim_end());
                return Ok(());
            };
            let target = discover::resolve(&targets, wanted)?;

            match (&compare, &baseline) {
                (Some((a, b)), _) => {
                    compare_baselines(&home, build_root, target, a, b, args, &_lock)
                }
                (None, Some(name)) => measure(&home, build_root, target, name, args, &_lock),
                // Unreachable: a named target with no `--compare` always
                // resolved a baseline above.
                (None, None) => Ok(()),
            }
        },
    )
}

/// Sample the current tree (or the `--commit` worktree) into a named baseline.
fn measure(
    home: &Path,
    build_root: &Path,
    target: &BenchTarget,
    baseline: &str,
    args: &BenchArgs,
    lock: &crate::lockfile::LockGuard,
) -> Result<(), DevError> {
    let mut criterion_args = vec!["--save-baseline".to_owned(), baseline.to_owned()];
    criterion_args.extend(args.args.iter().cloned());

    output::bench_msg(&format!(
        "measuring {} into baseline '{baseline}'",
        target.name
    ));
    let criterion_dir = criterion_dir(home);
    let before = saved_estimates(&criterion_dir, baseline);
    cargo_bench(home, build_root, target, &criterion_args, lock)?;

    // Criterion exits 0 when a filter matches no benchmark, so success alone
    // does not mean anything was measured. Each benchmark it measures rewrites
    // its `<baseline>/estimates.json`; none written means nothing ran, and a
    // stamp then would vouch for an empty (or older) baseline.
    let measured = rewritten(&before, &saved_estimates(&criterion_dir, baseline));
    if measured == 0 {
        return Err(DevError::Refused(format!(
            "cargo bench measured nothing into baseline '{baseline}' - no benchmark in {} \
             matched the arguments after `--`; nothing saved",
            target.name
        )));
    }

    // Written only after a run that measured something: a stamp for a baseline
    // that doesn't exist would make a later comparison claim comparability it
    // can't have.
    stamp::write(home, baseline, &stamp::Stamp::capture(build_root))?;
    output::bench_msg(&format!(
        "saved baseline '{baseline}' - compare with `brokkr bench {} --compare {baseline} <other>`",
        target.name
    ));
    Ok(())
}

/// Criterion's data directory under the bench home (its `CRITERION_HOME`).
fn criterion_dir(home: &Path) -> PathBuf {
    home.join("criterion")
}

/// Every `<baseline>/estimates.json` under `dir`, with its modification time:
/// the files a `--save-baseline <baseline>` run writes, one per benchmark.
/// Symlinks are not followed. An unreadable directory contributes nothing.
fn saved_estimates(dir: &Path, baseline: &str) -> HashMap<PathBuf, Option<SystemTime>> {
    let mut found = HashMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if !kind.is_dir() {
                continue;
            }
            let path = entry.path();
            if entry.file_name() == baseline {
                let estimates = path.join("estimates.json");
                if let Ok(meta) = std::fs::metadata(&estimates) {
                    found.insert(estimates, meta.modified().ok());
                }
            }
            pending.push(path);
        }
    }
    found
}

/// How many estimate files `after` holds that `before` did not, or holds
/// with a different modification time.
fn rewritten(
    before: &HashMap<PathBuf, Option<SystemTime>>,
    after: &HashMap<PathBuf, Option<SystemTime>>,
) -> usize {
    after
        .iter()
        .filter(|(path, modified)| before.get(*path) != Some(*modified))
        .count()
}

/// Diff two stored baselines without sampling.
fn compare_baselines(
    home: &Path,
    build_root: &Path,
    target: &BenchTarget,
    a: &str,
    b: &str,
    args: &BenchArgs,
    lock: &crate::lockfile::LockGuard,
) -> Result<(), DevError> {
    let a = &resolve_stored_baseline(home, build_root, a)?;
    let b = &resolve_stored_baseline(home, build_root, b)?;
    check_environments(home, a, b, args.lenient)?;

    // `--load-baseline` supplies the *new* side from storage; `--baseline` is
    // the reference. So `--compare A B` reads "B against A", matching the
    // argument order of `brokkr results --compare`.
    let mut criterion_args = vec![
        "--load-baseline".to_owned(),
        b.to_owned(),
        "--baseline".to_owned(),
        a.to_owned(),
    ];
    criterion_args.extend(args.args.iter().cloned());

    output::bench_msg(&format!("comparing '{b}' against '{a}' (no sampling)"));
    cargo_bench(home, build_root, target, &criterion_args, lock)
}

/// The recorded baseline `name` refers to.
///
/// A stored name is taken as-is. Otherwise, when `name` is a revision git can
/// resolve, the baseline recorded for that commit: first under the fixed-width
/// name ([`crate::git::Commit::short`]) a measuring run now saves under, then under
/// any shorter hash prefix of the same commit - the names `git rev-parse
/// --short` produced before the width was pinned, which grew with the repo. So
/// `--compare` takes a hash at whatever width it was copied, or a ref.
fn resolve_stored_baseline(home: &Path, build_root: &Path, name: &str) -> Result<String, DevError> {
    stamp::validate_label(name)?;
    if stamp::stamp_path(home, name).exists() {
        return Ok(name.to_owned());
    }
    let missing = || {
        DevError::Refused(format!(
            "no baseline '{name}' recorded; `brokkr bench --baselines` lists what exists"
        ))
    };
    let Ok(commit) = crate::git::resolve_commit(build_root, name) else {
        return Err(missing());
    };
    let short = commit.short();
    if stamp::stamp_path(home, short.as_str()).exists() {
        return Ok(short.as_str().to_owned());
    }
    let legacy: Vec<String> = stored_baseline_names(home)
        .into_iter()
        .filter(|n| {
            n.len() >= 4
                && n.bytes().all(|b| b.is_ascii_hexdigit())
                && commit.as_str().starts_with(n.as_str())
        })
        .collect();
    match legacy.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(missing()),
        many => Err(DevError::Refused(format!(
            "'{name}' matches several recorded baselines ({}); name one exactly",
            many.join(", ")
        ))),
    }
}

/// Every baseline name with a stamp on disk, unsorted.
fn stored_baseline_names(home: &Path) -> Vec<String> {
    match std::fs::read_dir(home.join("stamps")) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".txt").map(str::to_owned)
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Refuse a comparison across differing build environments.
///
/// Fails closed: the caller has already established that both stamps exist,
/// so a stamp that cannot be read now (permissions, I/O, removed in between)
/// is a comparability question left unanswered, not a pass. `--lenient`
/// downgrades that to a warning exactly as it does a mismatch.
fn check_environments(home: &Path, a: &str, b: &str, lenient: bool) -> Result<(), DevError> {
    let read = |name: &str| -> Result<stamp::Stamp, DevError> {
        stamp::read(home, name)?.ok_or_else(|| {
            DevError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("baseline stamp for '{name}' disappeared before it could be read"),
            ))
        })
    };
    let (sa, sb) = match (read(a), read(b)) {
        (Ok(sa), Ok(sb)) => (sa, sb),
        (Err(e), _) | (_, Err(e)) => {
            if lenient {
                output::warn(&format!(
                    "{e}\ncomparing anyway (--lenient); build environments were not checked"
                ));
                return Ok(());
            }
            return Err(DevError::Preflight(vec![
                e.to_string(),
                "the build environments cannot be checked, so a delta is not \
                 attributable to the code. Fix the stamp, or pass --lenient to \
                 compare regardless"
                    .into(),
            ]));
        }
    };
    let diffs = sa.differences(&sb);
    if diffs.is_empty() {
        return Ok(());
    }

    let mut lines = vec![format!(
        "'{a}' and '{b}' were measured under different conditions:"
    )];
    for (field, mine, theirs) in &diffs {
        lines.push(format!("  {field}: {a}={mine} / {b}={theirs}"));
    }
    if lenient {
        lines.push("comparing anyway (--lenient); the delta may be an artefact".into());
        output::warn(&lines.join("\n"));
        return Ok(());
    }
    lines.push(
        "a delta across these is not attributable to the code. Re-measure one \
         side, or pass --lenient to compare regardless"
            .into(),
    );
    Err(DevError::Preflight(lines))
}

/// Decide what to call the baseline this run produces.
///
/// A clean tree names itself: the short hash is stable, meaningful, and
/// reproducible from the git log. A dirty tree has no such name - and the
/// obvious fallback of `<hash>-dirty` is the worst option available, because
/// the edit-measure-edit-measure loop is the most common way to use this
/// command and every iteration would silently overwrite the last. So a dirty
/// tree must be named explicitly. `--name` is also how you label a baseline
/// you want to keep for reasons git can't express.
/// Both git questions are asked of the *build* root: that is the repo whose
/// commit names the baseline, and whose cleanliness decides whether a name can
/// be derived at all. The config directory above it may not even be a git repo.
fn resolve_baseline_name(
    build_root: &Path,
    included_config_files: &[PathBuf],
    args: &BenchArgs,
) -> Result<String, DevError> {
    if let Some(name) = &args.name {
        stamp::validate_label(name)?;
        return Ok(name.clone());
    }
    // A `--commit` run measures a fresh checkout of exactly that ref, so the
    // ref names the baseline and the working tree's state is irrelevant - there
    // is nothing uncommitted in a tree nobody has edited. Resolved against the
    // live repo, before the worktree exists.
    //
    // Named by the fixed-width abbreviation (`Commit::short`), so a commit's
    // baseline keeps its name as the repo grows and whatever ref or hash width
    // `--commit` was given.
    if let Some(commit) = &args.commit {
        return Ok(crate::git::resolve_commit(build_root, commit)?
            .short()
            .as_str()
            .to_owned());
    }
    let short = crate::git::resolve_commit(build_root, "HEAD")?
        .short()
        .as_str()
        .to_owned();
    if is_dirty(build_root, included_config_files)? {
        return Err(DevError::Preflight(vec![
            format!(
                "the tree has uncommitted changes, so '{short}' would not \
                 identify what was measured"
            ),
            "re-running after another edit would silently overwrite it".into(),
            "name this baseline with `--name <label>`, or commit first".into(),
        ]));
    }
    Ok(short)
}

/// True when a source or build-input change keeps a commit from naming the
/// measured binary. The shared tree snapshot also ignores prose, config
/// outputs, approved baselines, and an unchanged toolchain pin moved aside by
/// brokkr.
fn is_dirty(build_root: &Path, included_config_files: &[PathBuf]) -> Result<bool, DevError> {
    crate::git::TreeState::read_for_measurement(build_root, included_config_files)
        .map(|state| state.for_measurement())
        .ok_or_else(|| {
            DevError::Preflight(vec![format!(
                "could not determine git status for {}",
                build_root.display()
            )])
        })
}

/// Print the recorded baselines and the environment each was taken under.
fn list_baselines(home: &Path) -> Result<(), DevError> {
    let mut names = stored_baseline_names(home);
    if names.is_empty() {
        output::run_msg("no baselines recorded; `brokkr bench <target>` measures one");
        return Ok(());
    }
    names.sort();

    let mut msg = format!("{} baselines:\n", names.len());
    for name in &names {
        let summary = match stamp::read(home, name) {
            Ok(Some(s)) => s.render().lines().collect::<Vec<_>>().join(", "),
            Ok(None) => String::new(),
            Err(e) => format!("(unreadable: {e})"),
        };
        msg.push_str(&format!("  {name}  {summary}\n"));
    }
    output::run_msg(msg.trim_end());
    Ok(())
}

/// Spawn `cargo bench` with stdio inherited and `CRITERION_HOME` pointed at
/// brokkr's store.
///
/// Goes through the protected passthrough runner, not a bare `Command`: a bench
/// is the longest-running child brokkr spawns, so it is the worst one to leave
/// unkillable. Without this the cargo PID never reaches the lockfile, so
/// `brokkr lock` can't show it and `kill --hard` can't target it, and a SIGTERM
/// to brokkr orphans a benchmark that then runs on unattended.
fn cargo_bench(
    home: &Path,
    build_root: &Path,
    target: &BenchTarget,
    criterion_args: &[String],
    lock: &crate::lockfile::LockGuard,
) -> Result<(), DevError> {
    std::fs::create_dir_all(home)?;

    let shape = crate::build_purpose::resolve(crate::build_purpose::Request {
        purpose: crate::build_purpose::Purpose::Bench,
        profile: crate::build_purpose::Profile::Bench,
        packages: vec!["-p".into(), target.package.clone()],
        features: crate::build_purpose::Features::default(),
        unification: Vec::new(),
        targets: vec!["--bench".into(), target.name.clone()],
        env: vec![(
            "CRITERION_HOME".into(),
            criterion_dir(home).display().to_string(),
        )],
        build_root: Some(build_root),
    });
    let mut cargo_args = vec!["bench".into()];
    cargo_args.extend(shape.args);
    cargo_args.push("--no-fail-fast".into());
    if !criterion_args.is_empty() {
        cargo_args.push("--".into());
        cargo_args.extend(criterion_args.iter().cloned());
    }

    output::build_msg(&format!("cargo {}", cargo_args.join(" ")));

    if let Some((_, dir)) = shape.env.iter().find(|(key, _)| key == "CARGO_TARGET_DIR") {
        output::detail(&format!("isolating worktree build target dir -> {dir}"));
    }
    let env: Vec<(&str, &str)> = shape
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let arg_refs: Vec<&str> = cargo_args.iter().map(String::as_str).collect();
    let out = output::run_passthrough_in(
        "cargo",
        &arg_refs,
        Some(build_root),
        &env,
        output::SpawnProfile::Cargo { phase: None },
        Some(lock),
    )?;
    let Some(code) = out.failed_code(&[]) else {
        return Ok(());
    };
    // Cargo and criterion printed their own diagnostics; echoing the exit code
    // back as `cargo bench exited with code N` under `[error]` added nothing.
    Err(DevError::Reported(format!(
        "cargo bench failed (exit code {code})"
    )))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn estimate(dir: &Path, bench: &str, baseline: &str) -> PathBuf {
        let path = dir.join(bench).join(baseline).join("estimates.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{}").unwrap();
        path
    }

    /// A run whose filter matched nothing writes no estimates, so nothing
    /// counts as measured, while another baseline's files and an older copy of
    /// this one are not mistaken for this run's work.
    #[test]
    fn only_estimates_this_run_wrote_count_as_measured() {
        let dir = crate::test_scratch::scratch("bench_cmd", "measured");
        estimate(&dir, "group/old_bench", "abc123");
        estimate(&dir, "group/other", "def456");
        let before = saved_estimates(&dir, "abc123");
        assert_eq!(before.len(), 1);

        // An unmatched filter: nothing written.
        assert_eq!(rewritten(&before, &saved_estimates(&dir, "abc123")), 0);

        // A matched one: a new benchmark appears and the old one is rewritten.
        estimate(&dir, "group/new_bench", "abc123");
        let old = dir.join("group/old_bench/abc123/estimates.json");
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(later).unwrap();
        assert_eq!(rewritten(&before, &saved_estimates(&dir, "abc123")), 2);
    }
}
