// The replay recipe: how to launch a lane's executions again.
//
// A plan names the executions and the artifacts' identity, but a stored
// selection is only a list of names; running some of them again is a launch,
// and a launch has a shape - which cargo build produced the binaries, the cwd
// and environment each is started with, how many tests share a process, how
// many run at once. That shape is persisted with the lane, at the moment the
// lane is prepared, and replay reads it back. It NEVER resolves the shape from
// today's `brokkr.toml`: the config may have changed since, and a replay of a
// different shape would be evidence about something else.
//
// What a replay does NOT restore, and says so (the report and the docs repeat
// it):
//
// - the ENVIRONMENT is the recorded additions applied over the ambient
//   environment of the replaying process - the launch envelope
//   ([`DirectRuntime::envelope`]: cwd, loader path, `[env]`, `CARGO_PKG_*`,
//   build-script env) and the sweep env, `BROKKR_TEST_BIN_DIR` among it. It is
//   not a snapshot of the original process's environment. A fresh hold
//   capability and a fresh orphan-reap token are minted for the new
//   invocation, as every spawn does. The nextest engine builds its own
//   children, so there the recorded part is the sweep env plus a fingerprint
//   of what the engine resolves from cargo configuration at launch (the
//   discovered `[env]` tables and the target runner): replay resolves it again
//   and REFUSES on any difference, it never uses today's configuration in
//   place of the recorded one;
// - process history: a replayed test runs in a new process, so whatever an
//   earlier test initialised in the killed process (a global logger, a lazy
//   static) is not there;
// - external state the killed tests left: files, services, locks.

/// The recipe's schema version. A replay refuses a recipe it does not know,
/// rather than guess at fields it cannot read.
pub(crate) const REPLAY_VERSION: u32 = 2;

/// Which `#[ignore]`d tests a lane executes, resolved from the COMPLETE launch
/// argv (the sweep's own libtest args and anything forwarded after `--`), not
/// from the sweep alone: `brokkr check -- -- --ignored` lists only ignored
/// tests and must expect them to run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IgnoredMode {
    /// Ignored tests are listed but not run.
    #[default]
    Exclude,
    /// `--include-ignored`: every test runs, ignored ones too.
    Include,
    /// `--ignored`: only the ignored tests run.
    Only,
}

impl IgnoredMode {
    /// Whether the lane executes the ignored tests it selects.
    pub(crate) fn runs_ignored(self) -> bool {
        self != Self::Exclude
    }
}

/// The ignored mode a libtest argv selects. `--include-ignored` wins over
/// `--ignored` (libtest refuses both together, so such an argv never gets as
/// far as running); the value of `--skip` is never read as a flag.
pub(crate) fn effective_ignored_mode<'a>(args: impl IntoIterator<Item = &'a str>) -> IgnoredMode {
    let (mut include, mut only) = (false, false);
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a {
            "--skip" => {
                it.next();
            }
            "--include-ignored" => include = true,
            "--ignored" => only = true,
            _ => {}
        }
    }
    if include {
        IgnoredMode::Include
    } else if only {
        IgnoredMode::Only
    } else {
        IgnoredMode::Exclude
    }
}

/// The flag that reproduces `mode` on a replay argv.
fn ignored_flag(mode: IgnoredMode) -> Option<&'static str> {
    match mode {
        IgnoredMode::Exclude => None,
        IgnoredMode::Include => Some("--include-ignored"),
        IgnoredMode::Only => Some("--ignored"),
    }
}

/// How a lane's executions share processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReplayModel {
    /// One harness process per binary, all of that binary's tests in it, one
    /// at a time. Cargo's serial lane, and `brokkr test`'s harnesses. Never
    /// auto-isolated: the interaction between tests of one process is
    /// exactly what a serial shared-process lane exists to expose.
    SerialShared,
    /// One harness process per binary, binaries concurrently under the
    /// lane's budget.
    Parallel,
    /// One process per test.
    Isolated,
    /// The nextest engine, process-per-test.
    Nextest,
}

impl ReplayModel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::SerialShared => "serial_shared",
            Self::Parallel => "parallel",
            Self::Isolated => "isolated",
            Self::Nextest => "nextest",
        }
    }
}

/// A `build_packages` pre-build, as a command.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SupportBuild {
    pub(crate) package: String,
    /// The cargo argv, after `cargo`.
    pub(crate) args: Vec<String>,
}

/// One test executable and how it is launched.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct BinaryReplay {
    pub(crate) binary: TestBinary,
    /// The directory it is started in.
    pub(crate) cwd: String,
    /// The environment additions, complete: applied over the ambient
    /// environment, in order (a later entry wins).
    pub(crate) env: Vec<(String, String)>,
}

/// One cargo resolution of a lane.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ResolutionReplay {
    pub(crate) resolution: Option<String>,
    /// The cargo selection the resolution was built with: handed to the same
    /// `cargo test --no-run` the lane's own preparation ran.
    pub(crate) build_args: Vec<String>,
    pub(crate) binaries: Vec<BinaryReplay>,
}

/// Everything needed to launch a lane's executions again.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct LaneReplay {
    pub(crate) version: u32,
    /// Why this lane cannot be replayed, when it cannot (an environment that
    /// could not be reconstructed at preparation).
    pub(crate) refusal: Option<String>,
    pub(crate) model: ReplayModel,
    /// The effective ignored mode of the lane's complete launch argv.
    #[serde(default)]
    pub(crate) ignored_mode: IgnoredMode,
    /// The engine lane only: what the engine resolves from cargo
    /// configuration when it launches (the discovered `[env]` tables and the
    /// target runner), as lines. A replay resolves it again and refuses on a
    /// difference; it is never re-resolved into a different launch.
    #[serde(default)]
    pub(crate) engine_launch: Vec<String>,
    /// The effective `--test-threads` of a process that runs one test at a
    /// time; for the engine lane its in-flight count, `None` for the engine's
    /// default.
    pub(crate) test_threads: Option<u32>,
    /// The parallel lane's in-flight budget. The per-binary thread counts the
    /// executor allocated from it are NOT here (they come from measured costs
    /// at run time): they are journaled per binary as
    /// [`JournalRecord::ThreadAllocation`] and replayed from there.
    pub(crate) parallel_budget: Option<u32>,
    /// libtest execution options kept on replay, selection predicates
    /// removed (`--nocapture`, `--show-output`).
    pub(crate) harness_args: Vec<String>,
    /// The per-test cap the lane ran under, seconds. A `brokkr test
    /// --timeout` run raised it for one test, and a replay of that test keeps
    /// the cap it was given.
    pub(crate) per_test_ceiling_secs: u64,
    /// The env the lane's cargo invocations ran under.
    pub(crate) env: Vec<(String, String)>,
    pub(crate) support_builds: Vec<SupportBuild>,
    /// Target selectors the binaries were narrowed with after the build.
    pub(crate) target_filters: Vec<String>,
    pub(crate) runtime_fingerprint: Option<Vec<String>>,
    pub(crate) support_fingerprint: Vec<String>,
    pub(crate) resolutions: Vec<ResolutionReplay>,
}

/// The libtest options of a lane that survive into a replay: the ones that say
/// how tests run, not which. `--skip`, `--exact`, `--ignored`,
/// `--include-ignored` and positional filters are selection predicates, and a
/// replay selects by full name instead.
fn execution_options(libtest_args: &[String]) -> Vec<String> {
    libtest_args
        .iter()
        .filter(|a| matches!(a.as_str(), "--nocapture" | "--show-output"))
        .cloned()
        .collect()
}

/// The recipe for a prepared lane, or `None` for a lane with no inventory and
/// the doctest-only lane (which has no binaries to launch).
pub(crate) fn build_lane_replay(sweep: &ResolvedSweep, prepared: &PreparedLane) -> Option<LaneReplay> {
    if prepared.unavailable.is_some() || prepared.skipped.is_some() {
        return None;
    }
    let (model, test_threads) = match lane_kind(sweep) {
        LaneKind::DocOnly => return None,
        LaneKind::Serial => (ReplayModel::SerialShared, Some(1)),
        LaneKind::Parallel => (ReplayModel::Parallel, None),
        LaneKind::Isolated => (ReplayModel::Isolated, Some(1)),
        LaneKind::Nextest => (ReplayModel::Nextest, sweep.test_threads.filter(|n| *n >= 1)),
    };
    let mut refusal: Option<String> = None;
    let mut resolutions = Vec::new();
    for r in &prepared.resolutions {
        let mut binaries = Vec::new();
        for b in &r.binaries {
            let (cwd, env) = match &b.envelope {
                Some((cwd, env)) => (cwd.clone(), env.clone()),
                None if model == ReplayModel::Nextest => (String::new(), Vec::new()),
                None => {
                    refusal.get_or_insert_with(|| {
                        format!("the launch environment of {} was not recorded", b.unit.id())
                    });
                    (String::new(), Vec::new())
                }
            };
            binaries.push(BinaryReplay { binary: b.binary.clone(), cwd, env });
        }
        resolutions.push(ResolutionReplay {
            resolution: r.resolution.clone(),
            build_args: r.selection.clone(),
            binaries,
        });
    }
    Some(LaneReplay {
        version: REPLAY_VERSION,
        refusal,
        model,
        ignored_mode: prepared.ignored_mode,
        engine_launch: prepared.engine_launch.clone(),
        test_threads,
        parallel_budget: sweep.parallel_budget,
        // The complete argv: forwarded libtest args are execution options too.
        harness_args: execution_options(&lane_filter_args(sweep, &prepared.libtest_extra)),
        per_test_ceiling_secs: test_runner::TEST_TIMEOUT.as_secs(),
        env: prepared.env.env.clone(),
        support_builds: sweep
            .build_packages
            .iter()
            .map(|pkg| SupportBuild {
                package: pkg.clone(),
                args: pre_build_args(sweep, pkg, &prepared.env.allow_args),
            })
            .collect(),
        target_filters: prepared.target_filters.clone(),
        runtime_fingerprint: prepared.runtime_fingerprint.clone(),
        support_fingerprint: prepared.support_fingerprint.clone(),
        resolutions,
    })
}

/// The libtest argv that replays `names` in one process: full positive names
/// with `--exact`, no original selection predicate (an original `--skip`
/// would subtract from a list that is already exact), and the lane's
/// execution options. `--exact` is part of the argv this builds, never
/// appended to an original one: that would turn a substring filter into an
/// identity that matches nothing.
///
/// `names` must not be empty - an empty positive filter runs the whole
/// harness - which the callers guarantee by never launching a group without
/// names; it is asserted here too, by returning nothing to run.
pub(crate) fn libtest_replay_argv(lane: &LaneReplay, names: &[String], threads: u32) -> Option<Vec<String>> {
    if names.is_empty() {
        return None;
    }
    let mut args: Vec<String> = names.to_vec();
    args.push("--exact".into());
    args.push(format!("--test-threads={threads}"));
    args.extend(["-Z", "unstable-options", "--format", "json"].map(str::to_owned));
    args.extend(ignored_flag(lane.ignored_mode).map(str::to_owned));
    args.extend(lane.harness_args.iter().cloned());
    Some(args)
}

/// The libtest argv of one process-isolated replay of one test.
pub(crate) fn isolated_replay_argv(lane: &LaneReplay, name: &str) -> Vec<String> {
    let mut args: Vec<String> = vec!["--exact".into(), name.to_owned(), "--test-threads=1".into()];
    args.extend(["-Z", "unstable-options", "--format", "json"].map(str::to_owned));
    args.extend(ignored_flag(lane.ignored_mode).map(str::to_owned));
    args.extend(lane.harness_args.iter().cloned());
    args
}

/// The per-argument limit `execve` enforces on Linux (`MAX_ARG_STRLEN`, 32
/// pages).
const MAX_ARG_STRLEN: usize = 131_072;

/// The kernel's bound on arguments plus environment, from `sysconf`; the
/// historical 128 KiB when it cannot say.
fn system_arg_max() -> usize {
    // SAFETY: sysconf takes no pointers.
    let n = unsafe { libc::sysconf(libc::_SC_ARG_MAX) };
    usize::try_from(n).ok().filter(|n| *n > 0).unwrap_or(131_072)
}

/// What the kernel will be handed for a launch: every argument and every
/// environment string, NUL terminators and the pointer arrays included.
pub(crate) fn exec_footprint(argv: &[String], env_bytes: usize, env_count: usize) -> usize {
    let args: usize = argv.iter().map(|a| a.len() + 1 + std::mem::size_of::<usize>()).sum();
    args + env_bytes + env_count * std::mem::size_of::<usize>()
}

/// The ambient environment's contribution to a launch, as `(bytes, strings)`.
fn ambient_env_footprint() -> (usize, usize) {
    use std::os::unix::ffi::OsStrExt as _;
    std::env::vars_os().fold((0, 0), |(bytes, count), (k, v)| {
        (bytes + k.as_bytes().len() + v.as_bytes().len() + 2, count + 1)
    })
}

/// Whether `argv` can be launched with `env` added to the ambient
/// environment. The ambient and the recorded additions are summed, which
/// overstates a key present in both - the safe direction, and a replay that
/// refuses on it says what it measured.
pub(crate) fn argv_fits(argv: &[String], env: &[(String, String)]) -> Result<(), String> {
    argv_fits_within(argv, env, ambient_env_footprint(), system_arg_max())
}

/// [`argv_fits`] against stated limits.
fn argv_fits_within(
    argv: &[String],
    env: &[(String, String)],
    ambient: (usize, usize),
    limit: usize,
) -> Result<(), String> {
    if let Some(long) = argv.iter().find(|a| a.len() >= MAX_ARG_STRLEN) {
        return Err(format!(
            "an argument of {} bytes exceeds the {MAX_ARG_STRLEN}-byte limit on one argument",
            long.len()
        ));
    }
    let added: usize = env.iter().map(|(k, v)| k.len() + v.len() + 2).sum();
    let size = exec_footprint(argv, ambient.0 + added, ambient.1 + env.len());
    // Half the limit: the kernel counts the strings and also reserves stack
    // space against the same budget, and a launch refused with E2BIG after
    // the earlier groups ran would split a replay.
    if size > limit / 2 {
        return Err(format!(
            "the launch would take {size} bytes of arguments and environment against a budget of \
             {} (half of ARG_MAX, {limit})",
            limit / 2
        ));
    }
    Ok(())
}

/// A (resolution, binary) group of executions to replay together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplayGroup {
    pub(crate) resolution: Option<String>,
    pub(crate) unit: BinaryUnit,
    pub(crate) tests: Vec<String>,
}

/// The thread count each parallel binary actually ran with, keyed by (lane,
/// resolution, binary): read back from the journal's
/// [`JournalRecord::ThreadAllocation`] records.
pub(crate) type ThreadAllocations = BTreeMap<(usize, Option<String>, BinaryUnit), u32>;

/// Every allocation `records` journaled.
pub(crate) fn thread_allocations(records: &[JournalRecord]) -> ThreadAllocations {
    records
        .iter()
        .filter_map(|r| match r {
            JournalRecord::ThreadAllocation { lane, resolution, unit, threads } => {
                Some(((*lane, resolution.clone(), unit.clone()), *threads))
            }
            _ => None,
        })
        .collect()
}

/// The `--test-threads` a replay launches `group` with: a shared-process lane
/// its recorded policy, a parallel lane EXACTLY what the original executor
/// allocated that binary (a recomputation from the remaining tests would
/// change how many of them share the process at once). A parallel binary the
/// journal holds no allocation for cannot be replayed faithfully, so it is
/// refused.
pub(crate) fn group_threads(
    replay: &LaneReplay,
    lane: usize,
    group: &ReplayGroup,
    allocs: &ThreadAllocations,
) -> Result<u32, String> {
    if replay.model != ReplayModel::Parallel {
        return Ok(replay.test_threads.unwrap_or(1));
    }
    let Some(threads) = allocs.get(&(lane, group.resolution.clone(), group.unit.clone())).copied() else {
        return Err(format!(
            "{} ran in a parallel lane but the run recorded no thread allocation for it, so the \
             concurrency it ran under cannot be reproduced",
            group.unit.id()
        ));
    };
    let budget = replay.parallel_budget.unwrap_or(1).max(1);
    if threads == 0 || threads > budget {
        return Err(format!(
            "{} recorded {threads} threads against a lane budget of {budget}",
            group.unit.id()
        ));
    }
    Ok(threads)
}

/// Why a lane's selected executions cannot be replayed from its recipe, or
/// nothing when they can. Pure over the plan and the journal's thread
/// allocations: the report states these before anything is attempted, and a
/// replay re-checks them.
pub(crate) fn replay_refusals(lane: &LaneRecord, groups: &[ReplayGroup], allocs: &ThreadAllocations) -> Vec<String> {
    let label = &lane.label;
    let Some(replay) = &lane.replay else {
        return vec![format!(
            "{label}: the plan holds no replay recipe for this lane ({})",
            lane.unavailable.as_deref().unwrap_or("it was not recorded")
        )];
    };
    if replay.version != REPLAY_VERSION {
        return vec![format!(
            "{label}: its recipe is version {} and this brokkr replays version {REPLAY_VERSION}",
            replay.version
        )];
    }
    if let Some(reason) = &replay.refusal {
        return vec![format!("{label}: {reason}")];
    }
    if replay.per_test_ceiling_secs == 0 || replay.per_test_ceiling_secs > 280 {
        return vec![format!(
            "{label}: its recorded per-test cap of {}s is outside the 1-280s brokkr allows",
            replay.per_test_ceiling_secs
        )];
    }
    let mut out = Vec::new();
    for g in groups {
        let recorded = replay
            .resolutions
            .iter()
            .filter(|r| r.resolution == g.resolution)
            .flat_map(|r| r.binaries.iter())
            .find(|b| BinaryUnit::of(&b.binary) == g.unit);
        let Some(recorded) = recorded else {
            out.push(format!("{label}: the recipe does not hold {}", g.unit.id()));
            continue;
        };
        match replay.model {
            ReplayModel::SerialShared | ReplayModel::Parallel => {
                let threads = match group_threads(replay, lane.lane, g, allocs) {
                    Ok(t) => t,
                    Err(why) => {
                        out.push(format!("{label}: {why}"));
                        continue;
                    }
                };
                if let Some(argv) = libtest_replay_argv(replay, &g.tests, threads)
                    && let Err(why) = argv_fits(&argv, &recorded.env)
                {
                    // A shared process cannot be split: splitting changes
                    // which tests share a process, which is the semantics
                    // being replayed.
                    out.push(format!(
                        "{label}: {} runs {} in one process and the argv does not fit ({why}); \
                         splitting a shared process would change what is replayed",
                        g.unit.id(),
                        output::count(g.tests.len(), "test")
                    ));
                }
            }
            ReplayModel::Isolated => {
                // One process per test, but each launch still has to fit: a
                // refusal after the earlier tests ran would split the replay.
                for t in &g.tests {
                    if let Err(why) = argv_fits(&isolated_replay_argv(replay, t), &recorded.env) {
                        out.push(format!("{label}: {} {t}: the launch does not fit ({why})", g.unit.id()));
                    }
                }
            }
            ReplayModel::Nextest => {
                for t in &g.tests {
                    if let Err(why) = engine_filterset_name_ok(t) {
                        out.push(format!("{label}: {} {t}: {why}", g.unit.id()));
                    }
                }
                if let Err(why) = engine_filterset_name_ok(&g.unit.id()) {
                    out.push(format!("{label}: {}: {why}", g.unit.id()));
                }
            }
        }
    }
    out
}

/// The characters a name may carry into the engine's filterset text. Anything
/// else is refused rather than escaped: a wrongly escaped filter silently
/// changes which tests run (the rule `qualified_skip_filterset` keeps too).
fn engine_filterset_name_ok(name: &str) -> Result<(), String> {
    let ok = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-' | '/' | '.');
    if !name.is_empty() && name.chars().all(ok) {
        Ok(())
    } else {
        Err(format!(
            "`{name}` has characters the nextest filterset translation does not interpolate"
        ))
    }
}

/// The filterset selecting exactly these (binary id, test) executions: one
/// `binary_id(=ID) & test(=NAME)` conjunction per pair, joined by `|`. Both
/// halves, because two binaries of one package may define the same test path
/// and a name alone would replay the one that already passed.
pub(crate) fn engine_exact_filterset(pairs: &[(String, String)]) -> Result<String, String> {
    if pairs.is_empty() {
        return Err("no executions to select".into());
    }
    let mut terms = Vec::with_capacity(pairs.len());
    for (binary_id, test) in pairs {
        engine_filterset_name_ok(binary_id)?;
        engine_filterset_name_ok(test)?;
        terms.push(format!("(binary_id(={binary_id}) & test(={test}))"));
    }
    Ok(terms.join(" | "))
}

#[cfg(test)]
mod recipe_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn lane_replay(include_ignored: bool) -> LaneReplay {
        let mode = if include_ignored { IgnoredMode::Include } else { IgnoredMode::Exclude };
        lane_replay_with(mode)
    }

    fn lane_replay_with(ignored_mode: IgnoredMode) -> LaneReplay {
        LaneReplay {
            version: REPLAY_VERSION,
            refusal: None,
            model: ReplayModel::SerialShared,
            ignored_mode,
            engine_launch: Vec::new(),
            test_threads: Some(1),
            parallel_budget: None,
            harness_args: vec!["--nocapture".into()],
            per_test_ceiling_secs: 20,
            env: Vec::new(),
            support_builds: Vec::new(),
            target_filters: Vec::new(),
            runtime_fingerprint: None,
            support_fingerprint: Vec::new(),
            resolutions: Vec::new(),
        }
    }

    /// Validation case: the replay argv carries full names and `--exact`, and
    /// none of the original selection - a `--skip` in the sweep's libtest
    /// args never reaches it.
    #[test]
    fn replay_argv_is_full_names_with_exact_and_no_original_skip() {
        let sweep_args: Vec<String> =
            ["--skip", "slow::", "--include-ignored", "--nocapture", "tests::"].map(String::from).to_vec();
        assert_eq!(execution_options(&sweep_args), vec!["--nocapture".to_owned()]);

        let lane = lane_replay(false);
        let names = vec!["detector::beta".to_owned(), "detector::gamma".to_owned()];
        let argv = libtest_replay_argv(&lane, &names, 1).unwrap();
        assert_eq!(&argv[..2], names.as_slice());
        assert_eq!(argv[2], "--exact");
        assert!(argv.contains(&"--test-threads=1".to_owned()));
        assert!(argv.windows(2).any(|w| w == ["--format", "json"]));
        assert!(!argv.iter().any(|a| a == "--skip" || a == "slow::"), "{argv:?}");
        assert!(!argv.contains(&"--include-ignored".to_owned()));
        assert!(argv.contains(&"--nocapture".to_owned()));
        // A lane that lifted `#[ignore]` keeps the mode.
        let lifted = libtest_replay_argv(&lane_replay(true), &names, 1).unwrap();
        assert!(lifted.contains(&"--include-ignored".to_owned()));
    }

    /// Validation case: an empty selection never launches - an empty
    /// positive filter would run the whole harness.
    #[test]
    fn an_empty_selection_has_no_argv() {
        assert!(libtest_replay_argv(&lane_replay(false), &[], 1).is_none());
    }

    #[test]
    fn the_isolated_argv_is_one_exact_name() {
        let argv = isolated_replay_argv(&lane_replay(true), "a::b");
        assert_eq!(&argv[..3], ["--exact", "a::b", "--test-threads=1"]);
        assert!(argv.contains(&"--include-ignored".to_owned()));
    }

    /// The ignored mode is resolved from the complete launch argv - the
    /// sweep's own args and everything forwarded after `--` - in all three
    /// states, and `--skip`'s value is never read as a flag.
    #[test]
    fn the_ignored_mode_is_resolved_from_the_complete_argv() {
        let mode = |args: &[&str]| effective_ignored_mode(args.iter().copied());
        assert_eq!(mode(&[]), IgnoredMode::Exclude);
        assert_eq!(mode(&["tests::", "--nocapture"]), IgnoredMode::Exclude);
        assert_eq!(mode(&["--include-ignored"]), IgnoredMode::Include);
        assert_eq!(mode(&["--ignored"]), IgnoredMode::Only);
        assert_eq!(mode(&["--skip", "--ignored", "x"]), IgnoredMode::Exclude);
        assert!(IgnoredMode::Only.runs_ignored() && IgnoredMode::Include.runs_ignored());
        assert!(!IgnoredMode::Exclude.runs_ignored());
    }

    /// An only-ignored lane replays with `--ignored`, an include lane with
    /// `--include-ignored`, an exclude lane with neither.
    #[test]
    fn the_replay_argv_keeps_the_ignored_mode() {
        let names = vec!["a".to_owned()];
        for (mode, flag) in [
            (IgnoredMode::Only, Some("--ignored")),
            (IgnoredMode::Include, Some("--include-ignored")),
            (IgnoredMode::Exclude, None),
        ] {
            let lane = lane_replay_with(mode);
            for argv in [libtest_replay_argv(&lane, &names, 1).unwrap(), isolated_replay_argv(&lane, "a")] {
                let got: Vec<&str> =
                    argv.iter().map(String::as_str).filter(|a| matches!(*a, "--ignored" | "--include-ignored")).collect();
                assert_eq!(got, flag.into_iter().collect::<Vec<_>>(), "{mode:?}: {argv:?}");
            }
        }
    }

    /// An isolated lane's launches are held to the same argv-size bound as
    /// the shared-process groups, before anything runs.
    #[test]
    fn an_isolated_launch_that_does_not_fit_is_refused() {
        let unit = unit();
        let b = test_binary_for_tests("core", "test", "suite");
        let mut lane = lane_replay_with(IgnoredMode::Exclude);
        lane.model = ReplayModel::Isolated;
        lane.resolutions = vec![ResolutionReplay {
            resolution: None,
            build_args: Vec::new(),
            binaries: vec![BinaryReplay {
                binary: b,
                cwd: "/x/core".into(),
                env: vec![("BIG".into(), "v".repeat(16 * 1024 * 1024))],
            }],
        }];
        let record = LaneRecord { replay: Some(lane), ..LaneRecord::empty(0, "iso".into(), LaneKind::Isolated, "s".into()) };
        let group = ReplayGroup { resolution: None, unit, tests: vec!["a".into()] };
        let why = replay_refusals(&record, std::slice::from_ref(&group), &ThreadAllocations::new());
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("does not fit"), "{why:?}");
    }

    /// An argv past the system limit is refused with a measured reason, never
    /// split.
    #[test]
    fn an_argv_over_the_limit_is_refused() {
        let small = vec!["a::b".to_owned(), "--exact".to_owned()];
        assert!(argv_fits_within(&small, &[], (4_000, 40), 2_097_152).is_ok());
        let big: Vec<String> = (0..40_000).map(|i| format!("module::submodule::test_number_{i}")).collect();
        let err = argv_fits_within(&big, &[], (4_000, 40), 2_097_152).unwrap_err();
        assert!(err.contains("budget"), "{err}");
        let huge = vec!["x".repeat(MAX_ARG_STRLEN)];
        assert!(argv_fits_within(&huge, &[], (0, 0), 2_097_152).unwrap_err().contains("one argument"));
        // The recorded environment counts against the same budget.
        let env = vec![("K".to_owned(), "v".repeat(1_100_000))];
        assert!(argv_fits_within(&small, &env, (4_000, 40), 2_097_152).is_err());
    }

    #[test]
    fn the_engine_filterset_pairs_binary_and_name() {
        let pairs = vec![
            ("pkg::bin/tool".to_owned(), "t::a".to_owned()),
            ("pkg".to_owned(), "t::a".to_owned()),
        ];
        let set = engine_exact_filterset(&pairs).unwrap();
        assert_eq!(
            set,
            "(binary_id(=pkg::bin/tool) & test(=t::a)) | (binary_id(=pkg) & test(=t::a))"
        );
        assert!(engine_exact_filterset(&[]).is_err());
        let odd = vec![("pkg".to_owned(), "a b".to_owned())];
        assert!(engine_exact_filterset(&odd).unwrap_err().contains("characters"));
    }

    fn unit() -> BinaryUnit {
        BinaryUnit {
            package_id: "path+file:///x/core#core@0.1.0".into(),
            package: "core".into(),
            kind: "test".into(),
            target: "suite".into(),
        }
    }

    fn lane_with_recipe(replay: Option<LaneReplay>) -> LaneRecord {
        LaneRecord { replay, ..LaneRecord::empty(0, "default".into(), LaneKind::Serial, "s".into()) }
    }

    /// Concrete reasons, not a bare "unavailable".
    #[test]
    fn refusals_name_the_lane_and_the_reason() {
        let group = ReplayGroup { resolution: None, unit: unit(), tests: vec!["a".into()] };
        let no_allocs = ThreadAllocations::new();
        let none = replay_refusals(&lane_with_recipe(None), std::slice::from_ref(&group), &no_allocs);
        assert_eq!(none.len(), 1);
        assert!(none[0].starts_with("default:"), "{none:?}");

        let mut old = lane_replay(false);
        old.version = REPLAY_VERSION + 1;
        let r = replay_refusals(&lane_with_recipe(Some(old)), std::slice::from_ref(&group), &no_allocs);
        assert!(r[0].contains("version"), "{r:?}");

        let mut refused = lane_replay(false);
        refused.refusal = Some("the launch environment of core::suite was not recorded".into());
        let r = replay_refusals(&lane_with_recipe(Some(refused)), std::slice::from_ref(&group), &no_allocs);
        assert!(r[0].contains("not recorded"), "{r:?}");

        // A recipe that holds no such binary.
        let r = replay_refusals(&lane_with_recipe(Some(lane_replay(false))), std::slice::from_ref(&group), &no_allocs);
        assert!(r[0].contains("does not hold core::suite"), "{r:?}");
    }

    fn prepared_with(envelope: Option<(String, Vec<(String, String)>)>) -> PreparedLane {
        let b = test_binary_for_tests("core", "test", "suite");
        let mut p = PreparedLane::bare(LaneEnv {
            allow_args: vec!["--config".into(), "x=1".into()],
            project_env: Vec::new(),
            env: vec![("BROKKR_TEST_BIN_DIR".into(), "/t/debug".into())],
        });
        p.set_ignored_mode(IgnoredMode::Include);
        p.engine_launch = vec!["cargo-env {}".into()];
        p.libtest_extra = vec!["--show-output".into()];
        p.runtime_fingerprint = Some(vec!["build-script a".into()]);
        p.support_fingerprint = vec!["support server server /t/server h".into()];
        p.resolutions.push(PreparedResolution {
            resolution: None,
            selection: vec!["-p".into(), "core".into()],
            binaries: vec![PreparedBinary {
                unit: BinaryUnit::of(&b),
                binary: b,
                hash: "h".into(),
                envelope,
                unlisted: None,
                selected: vec!["t".into()],
                ignored: BTreeSet::new(),
                benchmarks: Vec::new(),
            }],
        });
        p
    }

    /// The recipe records what a launch is made of, from the prepared lane and
    /// the sweep - and nothing resolved later: the build selection, the env,
    /// the support builds, each binary's cwd and environment, the execution
    /// model, the thread policy and the execution options without a single
    /// selection predicate.
    #[test]
    fn a_recipe_records_the_launch_and_no_selection_predicate() {
        let sweep = ResolvedSweep {
            label: "serial".into(),
            libtest_args: ["--skip", "slow::", "--include-ignored", "--nocapture"].map(String::from).to_vec(),
            build_packages: vec!["server".into()],
            ..ResolvedSweep::default()
        };
        let prepared = prepared_with(Some(("/x/core".into(), vec![("CARGO_PKG_NAME".into(), "core".into())])));
        let r = build_lane_replay(&sweep, &prepared).expect("a recipe");
        assert_eq!(r.version, REPLAY_VERSION);
        assert_eq!(r.model, ReplayModel::SerialShared);
        assert_eq!(r.test_threads, Some(1));
        assert_eq!(r.ignored_mode, IgnoredMode::Include);
        assert_eq!(r.engine_launch, vec!["cargo-env {}".to_owned()]);
        // The sweep's own execution options and the forwarded ones.
        assert_eq!(r.harness_args, vec!["--nocapture".to_owned(), "--show-output".to_owned()]);
        assert_eq!(r.env, vec![("BROKKR_TEST_BIN_DIR".to_owned(), "/t/debug".to_owned())]);
        assert_eq!(r.runtime_fingerprint, Some(vec!["build-script a".to_owned()]));
        assert_eq!(r.support_fingerprint.len(), 1);
        assert_eq!(r.support_builds.len(), 1);
        assert_eq!(r.support_builds[0].package, "server");
        assert!(r.support_builds[0].args.windows(2).any(|w| w == ["--package", "server"]), "{:?}", r.support_builds);
        assert_eq!(r.resolutions[0].build_args, vec!["-p".to_owned(), "core".to_owned()]);
        assert_eq!(r.resolutions[0].binaries[0].cwd, "/x/core");
        assert_eq!(r.resolutions[0].binaries[0].env[0].0, "CARGO_PKG_NAME");
        assert!(r.refusal.is_none());
        // The persisted form carries no `--skip` anywhere.
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("slow::") && !json.contains("\"--skip\""), "{json}");

        // Each lane kind has its model; a doc-only lane has no recipe.
        let model = |sweep: &ResolvedSweep| {
            build_lane_replay(sweep, &prepared).map(|r| (r.model, r.test_threads, r.parallel_budget))
        };
        let parallel = ResolvedSweep { parallel_budget: Some(8), ..ResolvedSweep::default() };
        assert_eq!(model(&parallel), Some((ReplayModel::Parallel, None, Some(8))));
        let isolated = ResolvedSweep { process_isolation: true, ..ResolvedSweep::default() };
        assert_eq!(model(&isolated), Some((ReplayModel::Isolated, Some(1), None)));
        let engine = ResolvedSweep {
            harness: crate::config::Harness::Nextest,
            test_threads: Some(4),
            ..ResolvedSweep::default()
        };
        assert_eq!(model(&engine), Some((ReplayModel::Nextest, Some(4), None)));
        assert!(model(&ResolvedSweep { doc_only: true, ..ResolvedSweep::default() }).is_none());
    }

    /// A launch environment that was not recorded refuses the lane's replay,
    /// with the reason - except on the engine lane, whose launch env is the
    /// engine's own. A lane with no inventory has no recipe at all.
    #[test]
    fn an_unrecorded_launch_environment_refuses_the_replay() {
        let sweep = ResolvedSweep::default();
        let r = build_lane_replay(&sweep, &prepared_with(None)).unwrap();
        assert!(r.refusal.as_deref().unwrap().contains("core::suite"), "{:?}", r.refusal);
        let engine = ResolvedSweep { harness: crate::config::Harness::Nextest, ..ResolvedSweep::default() };
        assert!(build_lane_replay(&engine, &prepared_with(None)).unwrap().refusal.is_none());
        let blind = PreparedLane::without_inventory(LaneEnv::default(), "no".into());
        assert!(build_lane_replay(&sweep, &blind).is_none());
    }

    /// A parallel binary replays with exactly the thread count the original
    /// executor allocated it - journaled, not recomputed from the remaining
    /// tests (two unresolved tests of a binary that ran with one thread would
    /// otherwise replay with two). A binary with no journaled allocation, or
    /// one past the lane budget, is refused.
    #[test]
    fn a_parallel_group_replays_with_its_journaled_thread_count() {
        let mut replay = lane_replay(false);
        replay.model = ReplayModel::Parallel;
        replay.test_threads = None;
        replay.parallel_budget = Some(8);
        replay.resolutions = vec![ResolutionReplay {
            resolution: None,
            build_args: Vec::new(),
            binaries: vec![BinaryReplay {
                binary: test_binary_for_tests("core", "test", "suite"),
                cwd: "/x/core".into(),
                env: Vec::new(),
            }],
        }];
        let group = ReplayGroup { resolution: None, unit: unit(), tests: vec!["a".into(), "b".into()] };
        let records = vec![JournalRecord::ThreadAllocation {
            lane: 3,
            resolution: None,
            unit: unit(),
            threads: 1,
        }];
        let allocs = thread_allocations(&records);
        assert_eq!(group_threads(&replay, 3, &group, &allocs), Ok(1), "not 2, the number of remaining tests");
        let argv = libtest_replay_argv(&replay, &group.tests, group_threads(&replay, 3, &group, &allocs).unwrap()).unwrap();
        assert!(argv.contains(&"--test-threads=1".to_owned()), "{argv:?}");
        // Another lane's allocation is not this lane's.
        assert!(group_threads(&replay, 4, &group, &allocs).unwrap_err().contains("no thread allocation"));
        let over = thread_allocations(&[JournalRecord::ThreadAllocation {
            lane: 3,
            resolution: None,
            unit: unit(),
            threads: 9,
        }]);
        assert!(group_threads(&replay, 3, &group, &over).unwrap_err().contains("budget of 8"));
        // The refusal reaches the lane-level check the report and replay use.
        let mut record = lane_with_recipe(Some(replay));
        record.lane = 4;
        let why = replay_refusals(&record, std::slice::from_ref(&group), &allocs);
        assert!(why.iter().any(|w| w.contains("no thread allocation")), "{why:?}");
    }
}
