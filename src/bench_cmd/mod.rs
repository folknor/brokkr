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

use std::path::{Path, PathBuf};

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
    /// Compare two already-stored baselines, sampling nothing. clap's
    /// `num_args = 2` guarantees the pair when present.
    pub compare: Option<Vec<String>>,
    /// List the baselines brokkr has recorded.
    pub baselines: bool,
    /// Explicit baseline name, required when saving from a dirty tree.
    pub name: Option<String>,
    /// Downgrade an environment mismatch from a refusal to a warning.
    pub lenient: bool,
    /// Verbatim passthrough to the criterion harness.
    pub args: Vec<String>,
}

/// `.brokkr/bench` under the project root - the criterion home and the stamp
/// store both live here.
///
/// The project root is the directory holding `brokkr.toml`, which is what
/// anchors every other `.brokkr/` store (`results.db`, `sidecar.db`,
/// artefacts). Two consequences: baselines measured at different commits land
/// in one store rather than following each `--commit` worktree, and when the
/// config lives one level above a foreign checkout, the baselines stay in the
/// parent instead of appearing as untracked files in a repo that isn't ours.
fn bench_home(project_root: &Path) -> PathBuf {
    project_root.join(".brokkr").join("bench")
}

/// Entry point.
///
/// Detects the project itself rather than being handed one, because `bench`
/// works in a tree with no `brokkr.toml` at all - target discovery comes from
/// cargo metadata, and the config only supplies `disable_toolchain`.
pub fn run(args: &BenchArgs) -> Result<(), DevError> {
    // The two roots are distinct and both matter here. `project_root` holds
    // `brokkr.toml` and anchors everything brokkr owns, including `.brokkr/`;
    // `build_root` is the working directory where cargo and git run. They
    // differ when the config sits one level *above* cwd, which is the layout
    // for driving a checkout that isn't ours - and in that layout the whole
    // point is that brokkr's state stays in the parent and the foreign repo
    // stays clean. Anchoring the baseline store to the build root would write
    // into that repo and, worse, dirty the very tree whose cleanliness decides
    // whether a baseline may be saved.
    let (project, project_root, build_root, disable_toolchain) =
        match crate::project::detect_optional()? {
            Some(d) => (
                Some(d.project),
                d.project_root,
                d.build_root,
                d.config.disable_toolchain,
            ),
            None => {
                let cwd = std::env::current_dir()?;
                (None, cwd.clone(), cwd, false)
            }
        };
    let home = bench_home(&project_root);

    // Listing what we recorded needs neither cargo nor the lock.
    if args.baselines {
        return list_baselines(&home);
    }

    let compare = match args.compare.as_deref() {
        Some([a, b]) => Some((a.clone(), b.clone())),
        _ => None,
    };

    // `with_worktree` re-arms the toolchain-disable at the worktree before the
    // closure runs, so the lock taken *inside* moves that tree's pin aside
    // rather than the live root's. Hence lock-inside-closure, not outside.
    // `with_worktree` cuts its worktree from the build root when the two roots
    // differ, so the tree checked out is the code tree rather than the config
    // directory above it.
    let parent_build_root = (build_root != project_root).then_some(build_root.as_path());
    with_worktree(
        &project_root,
        parent_build_root,
        args.commit.as_deref(),
        false,
        disable_toolchain,
        |wt| {
            let build_root = wt.unwrap_or(&build_root);
            let _lock = acquire_cmd_lock_opt(project, build_root, "bench")?;

            let targets = discover::discover(build_root)?;
            if targets.is_empty() {
                return Err(DevError::Build(
                    "no bench targets in this workspace - nothing to benchmark".into(),
                ));
            }
            let Some(wanted) = args.target.as_deref() else {
                output::run_msg(discover::index(&targets).trim_end());
                return Ok(());
            };
            let target = discover::resolve(&targets, wanted)?;

            match &compare {
                Some((a, b)) => compare_baselines(&home, build_root, target, a, b, args),
                None => measure(&home, build_root, target, args),
            }
        },
    )
}

/// Sample the current tree (or the `--commit` worktree) into a named baseline.
fn measure(
    home: &Path,
    build_root: &Path,
    target: &BenchTarget,
    args: &BenchArgs,
) -> Result<(), DevError> {
    let baseline = resolve_baseline_name(build_root, args)?;

    let mut criterion_args = vec!["--save-baseline".to_owned(), baseline.clone()];
    criterion_args.extend(args.args.iter().cloned());

    output::bench_msg(&format!(
        "measuring {} into baseline '{baseline}'",
        target.name
    ));
    cargo_bench(home, build_root, target, &criterion_args)?;

    // Written only after a successful run: a stamp for a baseline that doesn't
    // exist would make a later comparison claim comparability it can't have.
    stamp::write(home, &baseline, &stamp::Stamp::capture(build_root))?;
    output::bench_msg(&format!(
        "saved baseline '{baseline}' - compare with `brokkr bench {} --compare {baseline} <other>`",
        target.name
    ));
    Ok(())
}

/// Diff two stored baselines without sampling.
fn compare_baselines(
    home: &Path,
    build_root: &Path,
    target: &BenchTarget,
    a: &str,
    b: &str,
    args: &BenchArgs,
) -> Result<(), DevError> {
    for name in [a, b] {
        if !stamp::stamp_path(home, name).exists() {
            return Err(DevError::Build(format!(
                "no baseline '{name}' recorded; `brokkr bench --baselines` lists what exists"
            )));
        }
    }
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
    cargo_bench(home, build_root, target, &criterion_args)
}

/// Refuse a comparison across differing build environments.
fn check_environments(home: &Path, a: &str, b: &str, lenient: bool) -> Result<(), DevError> {
    let (Some(sa), Some(sb)) = (stamp::read(home, a), stamp::read(home, b)) else {
        return Ok(());
    };
    let diffs = sa.differences(&sb);
    if diffs.is_empty() {
        return Ok(());
    }

    let mut lines =
        vec![format!("'{a}' and '{b}' were measured under different conditions:")];
    for (field, mine, theirs) in &diffs {
        lines.push(format!("  {field}: {a}={mine} / {b}={theirs}"));
    }
    if lenient {
        lines.push("comparing anyway (--lenient); the delta may be an artefact".into());
        output::error(&lines.join("\n"));
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
fn resolve_baseline_name(build_root: &Path, args: &BenchArgs) -> Result<String, DevError> {
    if let Some(name) = &args.name {
        return Ok(name.clone());
    }
    let short = git(build_root, &["rev-parse", "--short", "HEAD"])?;
    if is_dirty(build_root)? {
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

/// True when the tree has uncommitted tracked changes or untracked files.
fn is_dirty(build_root: &Path) -> Result<bool, DevError> {
    Ok(!git(build_root, &["status", "--porcelain"])?.is_empty())
}

fn git(dir: &Path, args: &[&str]) -> Result<String, DevError> {
    let captured = output::run_captured("git", args, dir)?;
    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!(
            "git {}: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&captured.stdout).trim().to_owned())
}

/// Print the recorded baselines and the environment each was taken under.
fn list_baselines(home: &Path) -> Result<(), DevError> {
    let dir = home.join("stamps");
    let mut names: Vec<String> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.strip_suffix(".txt").map(str::to_owned)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    if names.is_empty() {
        output::run_msg("no baselines recorded; `brokkr bench <target>` measures one");
        return Ok(());
    }
    names.sort();

    let mut msg = format!("{} baselines:\n", names.len());
    for name in &names {
        let summary = stamp::read(home, name)
            .map(|s| {
                s.render()
                    .lines()
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        msg.push_str(&format!("  {name}  {summary}\n"));
    }
    output::run_msg(msg.trim_end());
    Ok(())
}

/// Spawn `cargo bench` with stdio inherited and `CRITERION_HOME` pointed at
/// brokkr's store.
fn cargo_bench(
    home: &Path,
    build_root: &Path,
    target: &BenchTarget,
    criterion_args: &[String],
) -> Result<(), DevError> {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command as ProcCommand;

    std::fs::create_dir_all(home)?;

    let mut cargo_args: Vec<String> = vec![
        "bench".into(),
        "-p".into(),
        target.package.clone(),
        "--bench".into(),
        target.name.clone(),
        "--no-fail-fast".into(),
    ];
    if !criterion_args.is_empty() {
        cargo_args.push("--".into());
        cargo_args.extend(criterion_args.iter().cloned());
    }

    output::build_msg(&format!("cargo {}", cargo_args.join(" ")));

    let mut cmd = ProcCommand::new("cargo");
    cmd.args(&cargo_args);
    cmd.current_dir(build_root);
    cmd.env("CRITERION_HOME", home.join("criterion"));
    let status = cmd.status().map_err(|e| DevError::Subprocess {
        program: "cargo".into(),
        code: None,
        stderr: e.to_string(),
    })?;
    if status.success() {
        return Ok(());
    }
    match status.code() {
        Some(code) => Err(DevError::Subprocess {
            program: "cargo bench".into(),
            code: Some(code),
            stderr: String::new(),
        }),
        None => Err(DevError::Subprocess {
            program: "cargo bench".into(),
            code: None,
            stderr: format!("killed by signal {}", status.signal().unwrap_or(0)),
        }),
    }
}
