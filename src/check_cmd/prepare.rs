// The `prepare` phase: the whole invocation planned before any test runs.
//
// Two products come out of it, and they are kept apart in code because they are
// different claims.
//
// - The EXECUTION INVENTORY: every enumerable lane (serial, parallel,
//   isolated, nextest - doctests are not enumerable) is prepared up front, its
//   shape built (the same `cargo test --no-run` the lane itself uses), every
//   binary listed under the launch envelope execution will use, its selection
//   and the executions it implies recorded, a content hash of every test
//   executable taken. Prepared for EVERY run, not only certifying ones: the
//   whole invocation up front is what lets a kill in one binary name the
//   tests of the binaries and lanes after it. A lane that cannot be
//   attributed to binaries (cargo-mediated parallelism, a serial lane with no
//   harness shim) is the one thing a non-certifying run keeps that a
//   certifying one refuses; its inventory is reported unavailable, with the
//   reason, and nothing is made up for it.
// - The POLICY UNIVERSE: every test of each required shape, exclusions, filter
//   liveness. Only under `certifies = "complete"`, whose claim it is.
//
// The certifying restrictions (`LaneInputs::certifying`) are therefore a
// separate switch from preparing at all: refusing the unattributable lanes,
// enumerating universes, holding each lane to its plan before it runs.
//
// Preparing up front is also what separates the two claims the audit used to
// mix. Policy coverage is a property of what the profile SELECTS, so it must
// not depend on how far the test phase got: a lane a fail-fast never reached
// still selects its pairs, and its executions surface as unobserved rather
// than its pairs as orphaned. And it is what makes the audit possible after a
// watchdog kill at all - the old audit enumerated after the test phase, which
// the raised shutdown flag forbids.
//
// The lanes then CONSUME the prepared selection rather than re-enumerating.
// Where an up-front preparation failed, the lanes that need a selection to
// execute (parallel, isolated, nextest) prepare their own just before they
// run, through the same code.

/// The env a lane builds and runs under, and the lint allows its cargo
/// invocations carry. Decided once per lane: every cargo run of a lane - the
/// pre-build, the prebuild, the listing, the run - must see the same, or one
/// of them re-fingerprints the shape and rebuilds it.
#[derive(Debug, Clone, Default)]
pub(crate) struct LaneEnv {
    /// `--config` flags carrying `[lints] allow`, when the config layer is
    /// the live one ([`rustflags::plumbing`]).
    pub(crate) allow_args: Vec<String>,
    /// brokkr's own pairs ([`sweep_runtime_env`]).
    pub(crate) project_env: Vec<(String, String)>,
    /// The sweep's env over brokkr's ([`merged_env`]): what cargo sees.
    pub(crate) env: Vec<(String, String)>,
}

impl LaneEnv {
    pub(crate) fn refs(&self) -> Vec<(&str, &str)> {
        self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
    }
}

/// The run-wide inputs a lane needs to be prepared or executed.
pub(crate) struct LaneInputs<'a> {
    pub(crate) project: Option<Project>,
    pub(crate) project_root: &'a Path,
    pub(crate) state_root: &'a Path,
    pub(crate) target_dir: &'a Path,
    pub(crate) allow_flags: &'a [String],
    pub(crate) commands: bool,
    /// Under a complete profile: refuse what cannot be attributed, enumerate
    /// the policy universe, hold each lane to its plan before it runs. Not
    /// what makes a lane prepared - every run prepares its lanes.
    pub(crate) certifying: bool,
}

pub(crate) fn lane_env(inputs: &LaneInputs<'_>, sweep: &ResolvedSweep) -> LaneEnv {
    // A lint allow reaches this build through exactly one layer, and which
    // one is per sweep: a sweep carrying `rustflags` exports an env var, so
    // the env is live for it whatever the config chain says.
    let (env_allows, allow_args) =
        rustflags::plumbing(inputs.project_root, !sweep.rustflags.is_empty(), inputs.allow_flags);
    // Dev unless the sweep pinned a profile: BROKKR_TEST_BIN_DIR must name the
    // directory this sweep's own pre-build wrote into.
    let profile_dir = sweep
        .profile
        .map_or("debug", crate::config::SweepProfile::target_subdir);
    let project_env = sweep_runtime_env(sweep, inputs.project, inputs.target_dir, profile_dir, env_allows);
    let env = merged_env(&sweep.env, &project_env);
    LaneEnv { allow_args, project_env, env }
}

/// How a sweep's tests execute.
pub(crate) fn lane_kind(sweep: &ResolvedSweep) -> LaneKind {
    if sweep.harness == crate::config::Harness::Nextest {
        LaneKind::Nextest
    } else if sweep.doc_only {
        LaneKind::DocOnly
    } else if sweep.parallel_budget.is_some() {
        LaneKind::Parallel
    } else if sweep.process_isolation {
        LaneKind::Isolated
    } else {
        LaneKind::Serial
    }
}

/// One test binary as a lane will run it.
#[derive(Debug, Clone)]
pub(crate) struct PreparedBinary {
    pub(crate) binary: TestBinary,
    pub(crate) unit: BinaryUnit,
    /// Content hash (xxh3-64, hex): the artifact's identity in the plan.
    pub(crate) hash: String,
    /// Why this binary could not be listed, when it could not: it answers no
    /// libtest `--list` (a `harness = false` target, a custom harness), so
    /// the tests it holds cannot be named. Only a lane that is not
    /// certifying keeps such a binary - a serial lane runs it through cargo
    /// regardless - and its tests are outside the inventory, as doctests are.
    pub(crate) unlisted: Option<String>,
    /// The names the lane's filters select in this binary, package-qualified
    /// skips removed, in listing order.
    pub(crate) selected: Vec<String>,
    /// The `#[ignore]`d names among them.
    pub(crate) ignored: BTreeSet<String>,
    /// The `#[bench]` functions the filters admit: outside the claim, but run
    /// once in test mode, so their records are expected.
    pub(crate) benchmarks: Vec<String>,
}

impl PreparedBinary {
    /// The names this lane executes: its selection, less the ignored names
    /// unless the lane lifts `#[ignore]`.
    pub(crate) fn executed(&self, include_ignored: bool) -> impl Iterator<Item = &String> {
        self.selected
            .iter()
            .filter(move |n| include_ignored || !self.ignored.contains(*n))
    }
}

/// One cargo resolution of a lane.
#[derive(Debug, Clone)]
pub(crate) struct PreparedResolution {
    pub(crate) resolution: Option<String>,
    /// The selection the lane's prebuild ran with, built again to verify the
    /// artifacts before the lane executes.
    pub(crate) selection: Vec<String>,
    pub(crate) binaries: Vec<PreparedBinary>,
}

/// Everything a lane needs to execute without enumerating anything.
pub(crate) struct PreparedLane {
    pub(crate) env: LaneEnv,
    /// Whether the lane executes the ignored tests it selects, resolved from
    /// the lane's complete launch argv ([`args_run_ignored`]).
    pub(crate) include_ignored: bool,
    /// Names a package-qualified skip removed, for the lane's report.
    pub(crate) pkg_skipped: usize,
    pub(crate) resolutions: Vec<PreparedResolution>,
    /// Target selectors the binaries were narrowed with after the prebuild.
    pub(crate) target_filters: Vec<String>,
    /// The direct-execution lanes' launch envelope.
    pub(crate) runtime: Option<DirectRuntime>,
    pub(crate) nextest: Option<NextestPrepared>,
    /// Forwarded cargo args (refused under a complete profile), for repro lines.
    pub(crate) cargo_extra: Vec<String>,
    /// Forwarded libtest args, appended to every direct execution.
    pub(crate) libtest_extra: Vec<String>,
    /// Test binaries the prebuild produced before any narrowing.
    pub(crate) enumerated: usize,
    /// Prepared in the `prepare` phase: verify the artifacts again before
    /// running, since other lanes have built since.
    pub(crate) verify: bool,
    /// The runtime index's fingerprint ([`BuildRuntimeIndex::fingerprint`])
    /// the launch envelope was built from, on the lanes that execute binaries
    /// directly or through the shim. The lane's re-verification compares a
    /// fresh one, so a launch envelope built from facts that no longer hold
    /// is caught, not used.
    pub(crate) runtime_fingerprint: Option<Vec<String>>,
    /// The executables the lane's `build_packages` pre-builds produced, by
    /// package, target, path and content ([`support_fingerprint`]), hashed
    /// after the lane's own build. A support package outside the test
    /// selection reaches the tests through `BROKKR_TEST_BIN_DIR`, which no test
    /// build's artifact stream names - so [`Self::runtime_fingerprint`] cannot
    /// see it, and without this a support binary another lane rebuilt since
    /// the plan ran unverified.
    pub(crate) support_fingerprint: Vec<String>,
    /// A serial lane's `cargo test` selection reaches a member with a
    /// doctested target ([`DocFacts::obliges`]), so - where the lane carries
    /// doctests - it must present a rustdoc stream to have completed.
    pub(crate) doc_obligation: bool,
    /// Why this lane has no inventory, when it has none: its streams cannot be
    /// attributed to binaries. See [`LaneRecord::unavailable`].
    pub(crate) unavailable: Option<String>,
    /// Why this lane does not run in this invocation (a CLI `-p` its config
    /// rules out).
    pub(crate) skipped: Option<String>,
    /// Set when preparing this lane failed outside a certifying claim: what
    /// the attempt printed (held back) and the error's own text. The lane's
    /// run reports this instead of preparing again - a second preparation
    /// would execute every binary's `--list` a second time.
    pub(crate) prepare_failed: Option<PrepareFailure>,
}

/// A held-back preparation failure, kept so it is reported once, later.
#[derive(Debug, Clone)]
pub(crate) struct PrepareFailure {
    pub(crate) held: Vec<String>,
    pub(crate) message: String,
}

impl PreparedLane {
    /// The record of a lane whose preparation failed (see
    /// [`Self::prepare_failed`]).
    fn failed(failure: PrepareFailure) -> Self {
        Self { prepare_failed: Some(failure), ..Self::bare(LaneEnv::default()) }
    }

    pub(crate) fn bare(env: LaneEnv) -> Self {
        Self {
            env,
            include_ignored: false,
            pkg_skipped: 0,
            resolutions: Vec::new(),
            target_filters: Vec::new(),
            runtime: None,
            nextest: None,
            cargo_extra: Vec::new(),
            libtest_extra: Vec::new(),
            enumerated: 0,
            verify: false,
            runtime_fingerprint: None,
            support_fingerprint: Vec::new(),
            doc_obligation: false,
            unavailable: None,
            skipped: None,
            prepare_failed: None,
        }
    }

    /// A lane with no inventory, and why.
    fn without_inventory(env: LaneEnv, reason: String) -> Self {
        Self { unavailable: Some(reason), ..Self::bare(env) }
    }

    /// A lane this invocation does not run.
    fn skipped(env: LaneEnv, reason: String) -> Self {
        Self { skipped: Some(reason), ..Self::bare(env) }
    }

    /// The executable path -> unit index of one resolution: how a harness's
    /// handshake is attributed. A path from another lane or resolution does
    /// not resolve here, which is the point.
    pub(crate) fn units_by_path(&self, resolution: &Option<String>) -> HashMap<String, BinaryUnit> {
        self.resolutions
            .iter()
            .filter(|r| &r.resolution == resolution)
            .flat_map(|r| r.binaries.iter())
            .map(|b| (b.binary.executable.clone(), b.unit.clone()))
            .collect()
    }

    /// The executable path -> planned content hash of one resolution, which
    /// the strict shim checks a harness against at its handshake.
    pub(crate) fn hashes_by_path(&self, resolution: &Option<String>) -> HashMap<String, String> {
        self.resolutions
            .iter()
            .filter(|r| &r.resolution == resolution)
            .flat_map(|r| r.binaries.iter())
            .map(|b| (b.binary.executable.clone(), b.hash.clone()))
            .collect()
    }

    pub(crate) fn artifacts(&self) -> Vec<PlannedArtifact> {
        self.resolutions
            .iter()
            .flat_map(|r| {
                r.binaries.iter().map(|b| PlannedArtifact {
                    resolution: r.resolution.clone(),
                    unit: b.unit.clone(),
                    executable: b.binary.executable.clone(),
                    hash: b.hash.clone(),
                })
            })
            .collect()
    }
}

/// A resolution's selection, as the direct-execution lanes and the serial
/// lane's cargo command spell it: the sweep's whole selection when nothing
/// narrows it, else the one package REPLACING the sweep's own `-p` list (cargo
/// unions selection flags) - with the profile, the unification pin and the
/// features, which the parallel lane's per-package prebuild used to drop.
fn resolution_selection(sweep: &ResolvedSweep, scope: &[&str], resolution: Option<&str>) -> Vec<String> {
    match resolution {
        Some(pkg) => {
            let mut args = sweep_profile_args(sweep);
            args.extend(sweep.unification_args());
            args.extend(["-p".to_owned(), pkg.to_owned()]);
            args.extend(sweep.cargo_feature_args.iter().cloned());
            args
        }
        None => sweep_selection_args(sweep, scope),
    }
}

/// The libtest argv that selects what a lane runs: its positional filters,
/// its `skip`/`include_ignored` args, and any forwarded ones.
fn lane_filter_args(sweep: &ResolvedSweep, libtest_extra: &[String]) -> Vec<String> {
    let mut args: Vec<String> = sweep.name_filters.clone();
    args.extend(sweep.libtest_args.iter().cloned());
    args.extend(libtest_extra.iter().cloned());
    args
}

/// Whether a libtest argv executes `#[ignore]`d tests: `--include-ignored`
/// runs every test, `--ignored` runs only the ignored ones. The value of
/// `--skip` is never read as a flag.
fn args_run_ignored<'a>(args: impl IntoIterator<Item = &'a str>) -> bool {
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a {
            "--skip" => {
                it.next();
            }
            "--include-ignored" | "--ignored" => return true,
            _ => {}
        }
    }
    false
}

/// Whether a lane executes the ignored tests it selects, from its complete
/// launch argv: the sweep's positional filters and libtest args and whatever
/// was forwarded after `--` ([`lane_filter_args`]). Resolved from all of it,
/// because a forwarded `--ignored` changes which tests the lane executes
/// exactly as a profile's `include_ignored` does.
fn lane_runs_ignored(sweep: &ResolvedSweep, libtest_extra: &[String]) -> bool {
    let args = lane_filter_args(sweep, libtest_extra);
    args_run_ignored(args.iter().map(String::as_str))
}

/// The argv of a lane's ignored-only listing: its filter args with
/// `--ignored` added and `--include-ignored` taken out. libtest's `--list`
/// includes `#[ignore]`d names whatever the flags, so the ignored subset
/// comes from `--ignored`, which lists only them - and libtest refuses
/// `--ignored` beside `--include-ignored` outright, so a lane that lifts
/// `#[ignore]` would fail this listing, and with it its preparation. An
/// `--ignored` the lane already carries is replaced, not doubled: getopts
/// refuses a repeated flag.
fn ignored_listing_args<'a>(filter_args: &[&'a str]) -> Vec<&'a str> {
    let mut out: Vec<&str> = filter_args
        .iter()
        .copied()
        .filter(|a| !matches!(*a, "--include-ignored" | "--ignored"))
        .collect();
    out.push("--ignored");
    out
}

/// List one binary under a lane's filters: its selected names (qualified skips
/// out) and its ignored ones. `Ok(None)` = a listing failed, already reported.
#[allow(clippy::too_many_arguments)]
fn list_binary(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    binary: &TestBinary,
    filter_args: &[String],
    env_refs: &[(&str, &str)],
    runtime: &DirectRuntime,
    pkg_skipped: &mut usize,
) -> Result<Option<PreparedBinary>, DevError> {
    let args: Vec<&str> = filter_args.iter().map(String::as_str).collect();
    let Some(listed) = binary_list(binary, inputs.project_root, &args, env_refs, runtime)? else {
        return Ok(None);
    };
    let ignored_args = ignored_listing_args(&args);
    let Some(ignored) = binary_list(binary, inputs.project_root, &ignored_args, env_refs, runtime)? else {
        return Ok(None);
    };
    let mut selected = Vec::new();
    for t in listed.tests {
        if sweep.qualified_skips.iter().any(|q| q.matches(&binary.package, &t)) {
            *pkg_skipped += 1;
            continue;
        }
        selected.push(t);
    }
    let hash = hash_file(Path::new(&binary.executable))
        .map_err(|e| DevError::Build(format!("could not hash {}: {e}", binary.executable)))?;
    Ok(Some(PreparedBinary {
        unit: BinaryUnit::of(binary),
        binary: binary.clone(),
        hash,
        unlisted: None,
        selected,
        ignored: ignored.tests.into_iter().collect(),
        benchmarks: listed.benchmarks,
    }))
}

/// [`list_binary`] for a lane that is not certifying: a binary that cannot be
/// listed is kept, with no tests and the reason, rather than costing the lane
/// its whole inventory. The error `list_binary` would print is held back and
/// becomes the reason; the lane itself runs the binary through cargo as ever.
#[allow(clippy::too_many_arguments)]
fn list_binary_or_unlisted(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    binary: &TestBinary,
    filter_args: &[String],
    env_refs: &[(&str, &str)],
    runtime: &DirectRuntime,
    pkg_skipped: &mut usize,
) -> Result<PreparedBinary, DevError> {
    if inputs.certifying {
        // A certifying lane cannot keep an unenumerable binary (its tests
        // would be outside the claim): it lists every binary, a custom harness
        // included, and a listing that is not a libtest one fails the lane.
        return list_binary(inputs, sweep, binary, filter_args, env_refs, runtime, pkg_skipped)?
            .ok_or_else(|| not_prepared(sweep));
    }
    // The one place preparation executes binaries where nothing listed them
    // before: a serial lane under a partial profile ran them through cargo
    // only. A target whose manifest says `harness = false` is arbitrary code
    // that may ignore `--list` and run its real workload, so here it is never
    // executed to ask - the manifest already answers the question. (Every
    // lane that listed binaries before preparation existed still does.)
    if let Some(why) = custom_harness_reason(binary) {
        return unlisted_binary(binary, why);
    }
    let (listed, held) = output::capture_errors(|| {
        list_binary(inputs, sweep, binary, filter_args, env_refs, runtime, pkg_skipped)
    });
    if let Some(p) = listed? {
        return Ok(p);
    }
    unlisted_binary(binary, held.join(" - "))
}

/// Why `binary` is known not to be a libtest harness from its manifest alone,
/// without running it: `harness = false` (src/test_cmd/focused.rs
/// classification), or a manifest that cannot be read to tell - an unknown is
/// not run on the chance that it lists.
fn custom_harness_reason(binary: &TestBinary) -> Option<String> {
    match crate::test_cmd::focused::eligibility(std::slice::from_ref(binary)) {
        Ok(e) if e.excluded.is_empty() => None,
        Ok(_) => Some(
            "its manifest declares `harness = false`, so it is arbitrary code with no libtest listing; \
             it was not run to ask"
                .to_owned(),
        ),
        Err(e) => Some(format!("{e}; it was not run to ask")),
    }
}

/// A binary kept in the inventory with no tests and the reason: its identity
/// (content hash) is still recorded.
fn unlisted_binary(binary: &TestBinary, why: String) -> Result<PreparedBinary, DevError> {
    let hash = hash_file(Path::new(&binary.executable))
        .map_err(|e| DevError::Build(format!("could not hash {}: {e}", binary.executable)))?;
    Ok(PreparedBinary {
        unit: BinaryUnit::of(binary),
        binary: binary.clone(),
        hash,
        unlisted: Some(why),
        selected: Vec::new(),
        ignored: BTreeSet::new(),
        benchmarks: Vec::new(),
    })
}

/// A build or listing failure already printed: the lane cannot be prepared.
fn not_prepared(sweep: &ResolvedSweep) -> DevError {
    DevError::Reported(format!("sweep '{}' could not be prepared", sweep.label))
}

/// Prepare one lane: build its shape and list what it selects. `scope` is the
/// CLI `-p` intersection, `extra_args` the forwarded `-- ...` (both empty under
/// a complete profile, which refuses them).
pub(crate) fn prepare_lane(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    scope: &[&str],
    extra_args: &[String],
) -> Result<PreparedLane, DevError> {
    let env = lane_env(inputs, sweep);
    let kind = lane_kind(sweep);
    let mut prepared = match kind {
        LaneKind::Nextest => prepare_nextest(inputs, sweep, scope, extra_args, &env)?,
        LaneKind::DocOnly => {
            if inputs.certifying {
                refuse_unattributable_serial(inputs, sweep, &env, extra_args)?;
            }
            PreparedLane::bare(env)
        }
        LaneKind::Parallel | LaneKind::Isolated => prepare_direct(inputs, sweep, scope, extra_args, &env, kind)?,
        LaneKind::Serial => prepare_serial(inputs, sweep, scope, extra_args, &env)?,
    };
    prepared.verify = true;
    Ok(prepared)
}

/// Why a serial lane's harnesses cannot be attributed, as the short reason
/// the inventory reports and the longer refusal a certifying claim fails with.
struct Unattributable {
    short: String,
    refusal: String,
}

/// The two shapes of serial lane whose harnesses cannot be told apart:
/// certification needs every harness on its own stream, and neither gives it.
///
/// - Cargo-mediated parallelism (`test_threads` 0 or above 1) runs many tests
///   on one stream with no per-harness isolation; the parallel-binaries lane
///   (`parallel = { budget = N }`) runs them concurrently AND attributably.
/// - A shape where the shim cannot be installed (a configured runner, a cross
///   target, a configured rustdoc - `serial_shim_fallback`) leaves every
///   harness on cargo's one stream.
fn serial_unattributable(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    env: &LaneEnv,
    extra_args: &[String],
) -> Option<Unattributable> {
    if !sweep.doc_only && matches!(sweep.test_threads, Some(n) if n != 1) {
        return Some(Unattributable {
            short: "cargo-mediated parallelism (`test_threads` other than 1) shares one output \
                    stream across tests and harnesses, so no test can be attributed to a binary"
                .into(),
            refusal: format!(
                "sweep '{}' runs its tests in parallel through cargo (`test_threads` other than 1), \
                 which shares one output stream across tests and harnesses, so its executions cannot \
                 be attributed under a \"complete\" claim. Use `parallel = {{ budget = N }}` on the \
                 [[check]] entry instead - the parallel-binaries lane runs the binaries concurrently \
                 with each one on its own stream.",
                sweep.label
            ),
        });
    }
    let (cargo_extra, _) = split_extra_args(extra_args);
    serial_shim_fallback(inputs.project_root, cargo_extra, &env.project_env, &sweep.env).map(|reason| {
        Unattributable {
            short: format!(
                "harness isolation is unavailable ({reason}), so every harness shares cargo's one \
                 output stream and no test can be attributed to a binary"
            ),
            refusal: format!(
                "sweep '{}' cannot isolate its test harnesses ({reason}), so its executions cannot be \
                 attributed and a \"complete\" claim cannot be certified. Remove the override, or run \
                 this lane under a profile that certifies nothing.",
                sweep.label
            ),
        }
    })
}

/// Refuse, under a complete profile, a serial lane whose harnesses cannot be
/// attributed ([`serial_unattributable`]).
fn refuse_unattributable_serial(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    env: &LaneEnv,
    extra_args: &[String],
) -> Result<(), DevError> {
    match serial_unattributable(inputs, sweep, env, extra_args) {
        Some(u) => Err(DevError::Config(u.refusal)),
        None => Ok(()),
    }
}

/// The workspace facts that decide whether a serial lane's `cargo test` runs
/// doctests at all: which members a selection reaches, and which of them have
/// a target rustdoc tests. Read from `cargo metadata`, never inferred from the
/// lane's harnesses - `[lib] test = true, doctest = false` has a library
/// harness and no doctests, `[lib] test = false, doctest = true` doctests and
/// no library harness.
#[derive(Debug, Clone, Default)]
pub(crate) struct DocFacts {
    members: Vec<String>,
    default_members: Vec<String>,
    /// Member names with a target cargo doctests: `doctest = true` on a
    /// library of crate type `lib`, `rlib` or `proc-macro` (cargo's
    /// `Target::doctestable`; a `cdylib`/`staticlib`/`dylib`-only library has
    /// no rustdoc test pass).
    doctestable: BTreeSet<String>,
}

impl DocFacts {
    pub(crate) fn from_metadata(meta: &serde_json::Value) -> Self {
        let mut names: HashMap<&str, &str> = HashMap::new();
        let mut doctestable = BTreeSet::new();
        for pkg in meta.get("packages").and_then(serde_json::Value::as_array).into_iter().flatten() {
            let (Some(id), Some(name)) = (
                pkg.get("id").and_then(serde_json::Value::as_str),
                pkg.get("name").and_then(serde_json::Value::as_str),
            ) else {
                continue;
            };
            names.insert(id, name);
            let tested = pkg
                .get("targets")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .any(|t| {
                    let doctest = t.get("doctest").and_then(serde_json::Value::as_bool).unwrap_or(false);
                    let kinds = |key: &str| -> Vec<&str> {
                        t.get(key)
                            .and_then(serde_json::Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(serde_json::Value::as_str)
                            .collect()
                    };
                    let rustdoc_tests = kinds("crate_types")
                        .into_iter()
                        .chain(kinds("kind"))
                        .any(|k| matches!(k, "lib" | "rlib" | "proc-macro"));
                    doctest && rustdoc_tests
                });
            if tested {
                doctestable.insert(name.to_owned());
            }
        }
        let ids = |key: &str| -> Vec<String> {
            meta.get(key)
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .filter_map(|id| names.get(id).map(|n| (*n).to_owned()))
                .collect()
        };
        let members = ids("workspace_members");
        // An older cargo omits the default set; the whole workspace is then
        // the conservative reading (it can only add an obligation).
        let default_members = if meta.get("workspace_default_members").is_some() {
            ids("workspace_default_members")
        } else {
            members.clone()
        };
        Self { members, default_members, doctestable }
    }

    /// The members one resolution of a serial lane runs `cargo test` over -
    /// the same selection [`sweep_selection_args`] spells.
    fn selected<'a>(&'a self, sweep: &'a ResolvedSweep, scope: &'a [&'a str]) -> Vec<&'a str> {
        if !scope.is_empty() {
            return scope.to_vec();
        }
        if !sweep.packages.is_empty() {
            return sweep.packages.iter().map(String::as_str).collect();
        }
        if !sweep.test_exclude_packages.is_empty() {
            return self
                .members
                .iter()
                .map(String::as_str)
                .filter(|m| !sweep.test_exclude_packages.iter().any(|x| x == m))
                .collect();
        }
        self.default_members.iter().map(String::as_str).collect()
    }

    /// Whether this resolution's `cargo test` is obliged to run a rustdoc
    /// pass: some selected member has a doctested target.
    pub(crate) fn obliges(&self, sweep: &ResolvedSweep, scope: &[&str]) -> bool {
        self.selected(sweep, scope).iter().any(|p| self.doctestable.contains(*p))
    }
}

fn workspace_doc_facts(project_root: &Path, env_refs: &[(&str, &str)]) -> Result<DocFacts, DevError> {
    let captured = output::run_captured_with_env(
        "cargo",
        &["metadata", "--no-deps", "--format-version", "1"],
        project_root,
        env_refs,
    )?;
    if !captured.status.success() {
        return Err(DevError::Build(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&captured.stderr)
        )));
    }
    let val: serde_json::Value = serde_json::from_slice(&captured.stdout)
        .map_err(|e| DevError::Build(format!("cargo metadata output unparseable: {e}")))?;
    Ok(DocFacts::from_metadata(&val))
}

/// The serial lane's preparation: the same build `cargo test` will do, listed.
/// A lane whose harnesses cannot be attributed is refused under a certifying
/// claim and has no inventory otherwise - it still runs, through cargo.
fn prepare_serial(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    scope: &[&str],
    extra_args: &[String],
    env: &LaneEnv,
) -> Result<PreparedLane, DevError> {
    if let Some(u) = serial_unattributable(inputs, sweep, env, extra_args) {
        if inputs.certifying {
            return Err(DevError::Config(u.refusal));
        }
        return Ok(PreparedLane::without_inventory(env.clone(), u.short));
    }
    let (cargo_extra, libtest_extra) = split_extra_args(extra_args);
    let env_refs = env.refs();
    let owned: Vec<String> = scope.iter().map(|s| (*s).to_owned()).collect();
    let mut built: Vec<(Option<String>, Vec<String>, Vec<TestBinary>)> = Vec::new();
    let mut index = BuildRuntimeIndex::default();
    let doc_facts = workspace_doc_facts(inputs.project_root, &env_refs)?;
    let mut doc_obligation = false;
    for resolution in sweep.resolutions(&owned) {
        let run_scope: Vec<&str> = match &resolution {
            Some(pkg) => vec![pkg.as_str()],
            None => scope.to_vec(),
        };
        doc_obligation |= doc_facts.obliges(sweep, &run_scope);
        // Exactly the cargo-level argv `run_one_test_sweep` hands `cargo test`,
        // so the prebuild names the same units and the run is a no-op rebuild.
        let mut selection = env.allow_args.clone();
        selection.extend(sweep_selection_args(sweep, &run_scope));
        selection.extend(cargo_extra.iter().cloned());
        let Some((binaries, idx)) =
            test_binaries_with_runtime(inputs.project_root, &selection, &env_refs, inputs.commands)?
        else {
            return Err(not_prepared(sweep));
        };
        index.merge(idx);
        built.push((resolution, selection, binaries));
    }
    let fingerprint = Some(index.fingerprint()?);
    let runtime = DirectRuntime::load(inputs.project_root, &env_refs, index)?;
    let filter_args = lane_filter_args(sweep, libtest_extra);
    let mut prepared = PreparedLane::bare(env.clone());
    prepared.runtime_fingerprint = fingerprint;
    prepared.doc_obligation = doc_obligation;
    prepared.include_ignored = lane_runs_ignored(sweep, libtest_extra);
    prepared.libtest_extra = libtest_extra.to_vec();
    prepared.target_filters = sweep.cargo_test_filters.clone();
    for (resolution, selection, binaries) in built {
        prepared.enumerated += binaries.len();
        let mut out = Vec::new();
        for b in filter_binaries(&binaries, &prepared.target_filters) {
            out.push(list_binary_or_unlisted(
                inputs,
                sweep,
                b,
                &filter_args,
                &env_refs,
                &runtime,
                &mut prepared.pkg_skipped,
            )?);
        }
        prepared.resolutions.push(PreparedResolution { resolution, selection, binaries: out });
    }
    prepared.runtime = Some(runtime);
    Ok(prepared)
}

/// The direct-execution lanes' preparation (parallel and isolated): prebuild
/// each resolution, reconstruct cargo's launch contract, list every binary.
fn prepare_direct(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    scope: &[&str],
    extra_args: &[String],
    env: &LaneEnv,
    kind: LaneKind,
) -> Result<PreparedLane, DevError> {
    if kind == LaneKind::Isolated && !extra_args.is_empty() {
        return Err(DevError::Config(
            "`brokkr check -- …` extra args are not supported on a sweep with \
             `isolation = \"process\"` - the per-test invocations own their argv."
                .into(),
        ));
    }
    let (cargo_extra, libtest_extra) = split_extra_args(extra_args);
    // Selectors narrow the PLAN; the rest rides on the prebuild. See
    // `partition_target_selectors` for why mixing the two is a real bug.
    let (extra_selectors, cargo_extra) = partition_target_selectors(cargo_extra);
    reject_forwarded_selectors(sweep, &cargo_extra)?;
    reject_unsupported_forwarded(sweep, &cargo_extra)?;
    let env_refs = env.refs();
    // Direct execution would bypass a configured runner; refuse before building.
    refuse_configured_runner(inputs.project_root, &env_refs)?;

    let owned: Vec<String> = scope.iter().map(|s| (*s).to_owned()).collect();
    let mut built: Vec<(Option<String>, Vec<String>, Vec<TestBinary>)> = Vec::new();
    let mut index = BuildRuntimeIndex::default();
    // Under package mode one prebuild PER PACKAGE, never a batched multi-`-p`:
    // the batched graph is observably not the N independent resolutions the
    // mode exists for, so it would enumerate binaries no run uses.
    for resolution in sweep.resolutions(&owned) {
        let mut selection = env.allow_args.clone();
        selection.extend(resolution_selection(sweep, scope, resolution.as_deref()));
        selection.extend(extra_selectors.iter().cloned());
        selection.extend(cargo_extra.iter().cloned());
        let Some((binaries, idx)) =
            test_binaries_with_runtime(inputs.project_root, &selection, &env_refs, inputs.commands)?
        else {
            return Err(not_prepared(sweep));
        };
        index.merge(idx);
        built.push((resolution, selection, binaries));
    }
    let fingerprint = Some(index.fingerprint()?);
    let runtime = DirectRuntime::load(inputs.project_root, &env_refs, index)?;
    let mut prepared = PreparedLane::bare(env.clone());
    prepared.runtime_fingerprint = fingerprint;
    prepared.include_ignored = lane_runs_ignored(sweep, libtest_extra);
    // The sweep's own `--test` filters UNION with any the caller supplied,
    // matching cargo's semantics for repeated selection flags. Under package
    // mode the sweep's filters cannot ride the per-package prebuild (cargo
    // refuses a target a package lacks), so this is where they narrow.
    prepared.target_filters = sweep.cargo_test_filters.clone();
    prepared.target_filters.extend(extra_selectors.iter().cloned());
    prepared.cargo_extra = cargo_extra;
    prepared.libtest_extra = libtest_extra.to_vec();
    list_direct_binaries(inputs, sweep, built, &mut prepared, libtest_extra, &env_refs, &runtime)?;
    prepared.runtime = Some(runtime);
    Ok(prepared)
}

/// The listing half of [`prepare_direct`], behind a seam so it can be driven
/// without a cargo build: list every binary the lane selects into `prepared`.
///
/// Every binary is listed by executing `--list`, a `harness = false` target
/// included, exactly as these lanes did before preparation existed: a custom
/// harness that answers with a valid libtest listing is enumerated and run, and
/// one that does not fails the lane cleanly ([`binary_list`]). These lanes
/// drive every binary through libtest's protocol and run only the names a
/// listing returned, so keeping an unlistable binary as unlisted would skip the
/// target silently.
fn list_direct_binaries(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    built: Vec<(Option<String>, Vec<String>, Vec<TestBinary>)>,
    prepared: &mut PreparedLane,
    libtest_extra: &[String],
    env_refs: &[(&str, &str)],
    runtime: &DirectRuntime,
) -> Result<(), DevError> {
    let filter_args = lane_filter_args(sweep, libtest_extra);
    for (resolution, selection, binaries) in built {
        prepared.enumerated += binaries.len();
        let mut out = Vec::new();
        for b in filter_binaries(&binaries, &prepared.target_filters) {
            let Some(p) = list_binary(inputs, sweep, b, &filter_args, env_refs, runtime, &mut prepared.pkg_skipped)?
            else {
                return Err(not_prepared(sweep));
            };
            out.push(p);
        }
        prepared.resolutions.push(PreparedResolution { resolution, selection, binaries: out });
    }
    Ok(())
}

/// Build a lane's shape again and prove what it is about to run is what was
/// planned. A no-op build normally (and the rebuild re-uplifts the support
/// binaries a later lane's build may have replaced). Under a certifying claim
/// a difference is a hard error for the lane, never a silent re-plan - a plan
/// that follows the artifacts around certifies whatever happens to be on disk.
///
/// What is verified, precisely: every test executable by path and content,
/// and - where the lane prepared one - the runtime index's fingerprint
/// ([`BuildRuntimeIndex::fingerprint`]: build-script env, out dirs and
/// link-search dirs, support bins by path and content), which is everything
/// the launch envelope is derived from besides the lane's own env. Checked
/// once EVERY resolution of the lane has been rebuilt, since one resolution's
/// build can rewrite another's artifacts in place, and nothing builds between
/// this check and the lane's first launch. What is not verified: the contents
/// of link-search directories (dylibs a build script placed there), which no
/// artifact stream names. A serial lane's harnesses are checked once more at
/// their handshake, immediately before each runs (`StrictPlan`), since its
/// `cargo test` builds again on its own.
///
/// `support` is what the lane's `build_packages` pre-builds just produced
/// ([`run_sweep_pre_build`]); hashed here, after the lane's builds, and held
/// to [`PreparedLane::support_fingerprint`].
///
/// Returns the differences rather than failing on them: a certifying lane
/// turns any into [`drift_error`], and a lane that is not certifying is not
/// refused for a plan that went stale (see `run_test_lane`). Operational
/// failures (a build that does not build) are errors.
pub(crate) fn lane_drift_of(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    prepared: &PreparedLane,
    support: &[SupportArtifact],
) -> Result<Vec<String>, DevError> {
    let env_refs = prepared.env.refs();
    lane_drift_with(
        sweep,
        prepared,
        |r| test_binaries_with_runtime(inputs.project_root, &r.selection, &env_refs, inputs.commands),
        support,
    )
}

/// The error a lane fails with when its artifacts are not the planned ones.
fn drift_error(label: &str, drift: &[String]) -> DevError {
    DevError::Build(format!(
        "sweep '{label}': artifacts changed since the plan - {}. The lane would run binaries the plan \
         never enumerated, so it does not run at all.",
        drift.join("; ")
    ))
}

/// A lane's drift as the error a certifying lane fails with, over a given
/// build step: how the tests drive the comparison without cargo.
#[cfg(test)]
fn verify_lane_with(
    sweep: &ResolvedSweep,
    prepared: &PreparedLane,
    build: impl FnMut(&PreparedResolution) -> Result<Option<(Vec<TestBinary>, BuildRuntimeIndex)>, DevError>,
    support: &[SupportArtifact],
) -> Result<(), DevError> {
    match lane_drift_with(sweep, prepared, build, support)? {
        drift if drift.is_empty() => Ok(()),
        drift => Err(drift_error(&sweep.label, &drift)),
    }
}

/// [`lane_drift_of`] over a given build step.
fn lane_drift_with(
    sweep: &ResolvedSweep,
    prepared: &PreparedLane,
    mut build: impl FnMut(&PreparedResolution) -> Result<Option<(Vec<TestBinary>, BuildRuntimeIndex)>, DevError>,
    support: &[SupportArtifact],
) -> Result<Vec<String>, DevError> {
    let resolutions: Vec<Option<String>> = prepared.resolutions.iter().map(|r| r.resolution.clone()).collect();
    let check = DriftCheck {
        artifacts: prepared.artifacts(),
        target_filters: &prepared.target_filters,
        runtime_fingerprint: prepared.runtime_fingerprint.as_ref(),
        support_fingerprint: &prepared.support_fingerprint,
    };
    match check.run(&resolutions, |i| build(&prepared.resolutions[i]), support)? {
        Some(drift) => Ok(drift),
        None => Err(not_prepared(sweep)),
    }
}

/// What a lane was planned as, to be held against a fresh build of it: the
/// one definition of "the same artifacts" a lane's pre-run verification uses.
pub(crate) struct DriftCheck<'a> {
    pub(crate) artifacts: Vec<PlannedArtifact>,
    pub(crate) target_filters: &'a [String],
    pub(crate) runtime_fingerprint: Option<&'a Vec<String>>,
    pub(crate) support_fingerprint: &'a [String],
}

impl DriftCheck<'_> {
    /// Rebuild every resolution (`build(i)` is the i-th of `resolutions`), then
    /// hash, then compare. `Ok(None)` is a build that failed, already reported.
    pub(crate) fn run(
        &self,
        resolutions: &[Option<String>],
        mut build: impl FnMut(usize) -> Result<Option<(Vec<TestBinary>, BuildRuntimeIndex)>, DevError>,
        support: &[SupportArtifact],
    ) -> Result<Option<Vec<String>>, DevError> {
        // Every build first, then every hash: hashing between builds would
        // certify a file the next resolution's build then rewrote.
        let mut rebuilt: Vec<(Option<String>, Vec<TestBinary>)> = Vec::new();
        let mut index = BuildRuntimeIndex::default();
        for (i, resolution) in resolutions.iter().enumerate() {
            let Some((binaries, idx)) = build(i)? else {
                return Ok(None);
            };
            index.merge(idx);
            rebuilt.push((resolution.clone(), binaries));
        }
        let mut current: Vec<(Option<String>, String, String)> = Vec::new();
        for (resolution, binaries) in &rebuilt {
            for b in filter_binaries(binaries, self.target_filters) {
                let hash = hash_file(Path::new(&b.executable))
                    .map_err(|e| DevError::Build(format!("could not hash {}: {e}", b.executable)))?;
                current.push((resolution.clone(), b.executable.clone(), hash));
            }
        }
        Ok(Some(self.compare(&current, &index.fingerprint()?, &support_fingerprint(support)?)))
    }

    /// Hold what a build produced to what was planned: `current` is every
    /// executable by (resolution, path, content hash), `runtime_now` the
    /// runtime index's fingerprint, `support_now` the support builds'.
    fn compare(
        &self,
        current: &[(Option<String>, String, String)],
        runtime_now: &[String],
        support_now: &[String],
    ) -> Vec<String> {
        let mut drift = artifact_drift(&self.artifacts, current);
        if let Some(planned) = self.runtime_fingerprint {
            for line in planned.iter().filter(|l| !runtime_now.contains(l)) {
                drift.push(format!("the launch envelope's `{line}` no longer holds"));
            }
            for line in runtime_now.iter().filter(|l| !planned.contains(l)) {
                drift.push(format!("the launch envelope gained `{line}`"));
            }
        }
        for line in self.support_fingerprint.iter().filter(|l| !support_now.contains(l)) {
            drift.push(format!("the support build's `{line}` no longer holds"));
        }
        for line in support_now.iter().filter(|l| !self.support_fingerprint.contains(l)) {
            drift.push(format!("the support build gained `{line}`"));
        }
        drift
    }
}

/// One shape resolution's universe: every binary, all its names, its ignored
/// names.
#[derive(Debug, Clone, Default)]
pub(crate) struct Universe {
    pub(crate) binaries: Vec<(BinaryUnit, Vec<String>, BTreeSet<String>)>,
}

/// Enumerate a shape resolution's universe: the bare shape selection (no
/// target filters - those narrow lanes, and are audited through their
/// selections), listed `--include-ignored` with no filters, and `--ignored`
/// for the ignored subset.
pub(crate) fn enumerate_universe(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    resolution: Option<&str>,
    env: &LaneEnv,
) -> Result<Universe, DevError> {
    let env_refs = env.refs();
    let bare = resolution_enumeration_args(sweep, resolution, env.allow_args.clone());
    let Some((binaries, index)) = test_binaries_with_runtime(inputs.project_root, &bare, &env_refs, inputs.commands)?
    else {
        return Err(DevError::Reported(format!("the universe of sweep '{}' could not be enumerated", sweep.label)));
    };
    let runtime = DirectRuntime::load(inputs.project_root, &env_refs, index)?;
    universe_from_binaries(inputs, sweep, &binaries, &env_refs, &runtime)
}

/// The listing half of [`enumerate_universe`], behind a seam so it can be
/// driven without a cargo build. A complete profile needs every test of the
/// shape named, and the only way to name a custom harness's tests is to run
/// it with `--list`, as enumeration always has: a valid listing is accepted, and
/// anything else fails the enumeration cleanly ([`binary_list`]) rather than
/// contribute an empty set.
fn universe_from_binaries(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    binaries: &[TestBinary],
    env_refs: &[(&str, &str)],
    runtime: &DirectRuntime,
) -> Result<Universe, DevError> {
    let mut out = Universe::default();
    for b in binaries {
        let Some(all) = binary_list(b, inputs.project_root, &["--include-ignored"], env_refs, runtime)? else {
            return Err(DevError::Reported(format!("the universe of sweep '{}' could not be listed", sweep.label)));
        };
        let Some(ignored) = binary_list(b, inputs.project_root, &["--ignored"], env_refs, runtime)? else {
            return Err(DevError::Reported(format!("the universe of sweep '{}' could not be listed", sweep.label)));
        };
        out.binaries.push((BinaryUnit::of(b), all.tests, ignored.tests.into_iter().collect()));
    }
    Ok(out)
}

/// The two operations preparation is made of, behind a seam so the plan's
/// assembly - markers, completeness, the stop on a watchdog - is testable
/// without cargo.
pub(crate) trait Preparer {
    /// Build the lane's declared support binaries (`build_packages`). Runs
    /// before the lane's listings: a binary whose static constructor or
    /// custom harness reads a support binary must be listed with the one the
    /// lane will run with, not a stale one or none. Returns the executables
    /// they produced, which the plan hashes once the lane is built.
    fn support(&mut self, sweep: &ResolvedSweep) -> Result<Vec<SupportArtifact>, DevError>;
    fn lane(&mut self, sweep: &ResolvedSweep) -> Result<PreparedLane, DevError>;
    fn universe(&mut self, sweep: &ResolvedSweep, resolution: Option<&str>, env: &LaneEnv) -> Result<Universe, DevError>;
}

/// The real preparer: cargo, listings, hashes.
pub(crate) struct CargoPreparer<'a> {
    pub(crate) inputs: LaneInputs<'a>,
    /// The CLI `-p` set, which each lane intersects with its own scope as the
    /// test phase does ([`cli_package_scope`]). Empty under a certifying
    /// claim, which refuses it.
    pub(crate) packages: &'a [String],
    /// The forwarded `-- …` args, which narrow what a lane selects and so
    /// what it is expected to execute. Empty under a certifying claim.
    pub(crate) extra_args: &'a [String],
}

impl Preparer for CargoPreparer<'_> {
    fn support(&mut self, sweep: &ResolvedSweep) -> Result<Vec<SupportArtifact>, DevError> {
        let env = lane_env(&self.inputs, sweep);
        let mut out = Vec::new();
        for pkg in &sweep.build_packages {
            out.extend(run_sweep_pre_build(
                self.inputs.project_root,
                sweep,
                pkg,
                &env.project_env,
                &env.allow_args,
                self.inputs.commands,
            )?);
        }
        Ok(out)
    }

    fn lane(&mut self, sweep: &ResolvedSweep) -> Result<PreparedLane, DevError> {
        // The same scope the test phase will hand the lane: a lane the CLI
        // `-p` rules out does not run, so it expects nothing.
        match cli_package_scope(sweep, self.packages, true) {
            Ok((scope, _)) => prepare_lane(&self.inputs, sweep, &scope, self.extra_args),
            Err(reason) => Ok(PreparedLane::skipped(lane_env(&self.inputs, sweep), reason)),
        }
    }

    fn universe(&mut self, sweep: &ResolvedSweep, resolution: Option<&str>, env: &LaneEnv) -> Result<Universe, DevError> {
        enumerate_universe(&self.inputs, sweep, resolution, env)
    }
}

/// The prepared profile: the plan, and each lane's executable preparation.
pub(crate) struct ProfilePrep {
    pub(crate) plan: AccountingPlan,
    pub(crate) lanes: Vec<Option<PreparedLane>>,
    /// The first lane that failed to prepare, when one did.
    pub(crate) error: Option<DevError>,
}

/// Prepare every active sweep and assemble the plan.
///
/// A lane that cannot be prepared is a marker on the plan, and preparation
/// goes on with the rest - every lane's refusal or build failure is reported
/// in one run - except on a stop: an interrupt or the phase watchdog ends it
/// at once, since every further spawn is refused anyway. Either way the plan
/// comes back, `complete = false` with its markers, so the audit still reports
/// what is known.
///
/// `certifying` is whether the run is held to a `certifies = "complete"`
/// claim: only then are policy universes enumerated and filters judged for
/// liveness. Every run gets the execution inventory. A lane that fails to
/// prepare under a claim that does not need it is a lane with no inventory
/// (its `unavailable` says why), not a refusal: it still runs.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub(crate) fn prepare_profile(
    sweeps: &[ResolvedSweep],
    doctests: bool,
    certifying: bool,
    preparer: &mut dyn Preparer,
) -> ProfilePrep {
    let mut plan = AccountingPlan { run_id: new_run_id(), certifying, ..AccountingPlan::default() };
    let mut lanes: Vec<Option<PreparedLane>> = Vec::new();
    let mut error: Option<DevError> = None;
    let mut universes: BTreeMap<(String, Option<String>), Option<Universe>> = BTreeMap::new();
    let mut shape_order: Vec<(String, Option<String>, usize)> = Vec::new();
    let mut ledger = FilterLedger::default();
    let mut stopped = false;

    for (idx, sweep) in sweeps.iter().enumerate() {
        let kind = lane_kind(sweep);
        let shape = shape_id(sweep);
        let mut record = LaneRecord {
            include_ignored: args_run_ignored(sweep.libtest_args.iter().map(String::as_str)),
            doc_carrier: lane_runs_doctests(sweep, doctests),
            // A doc-only lane always; a serial carrier once its binaries are
            // known to hold a library (below).
            doc_streams_required: kind == LaneKind::DocOnly,
            ..LaneRecord::empty(idx, sweep.label.clone(), kind, shape.clone())
        };
        if stopped || crate::shutdown::is_shutdown_requested() {
            if !stopped {
                plan.incomplete.push("preparation stopped before every lane was prepared".into());
            }
            stopped = true;
            if certifying {
                for f in &sweep.declared_filters {
                    ledger.record_unknown(f, &sweep.label);
                }
            }
            record.unavailable = Some("preparation was stopped before this lane".into());
            plan.lanes.push(record);
            lanes.push(None);
            continue;
        }
        // The support executables are hashed AFTER the lane's own build, the
        // order the lane's verification repeats: that build can re-uplift a
        // support bin over the pre-build's, and the file the tests read is
        // whichever landed last.
        let attempt = |preparer: &mut dyn Preparer| {
            preparer.support(sweep).and_then(|support| {
                let mut p = preparer.lane(sweep)?;
                p.support_fingerprint = support_fingerprint(&support)?;
                Ok(p)
            })
        };
        // Outside a certifying claim a lane that will not prepare is not yet a
        // failure: it runs anyway (a serial lane through cargo, the others by
        // preparing themselves again, printing their own error then). What
        // the attempt would have printed is held back, kept in the run log,
        // and becomes the reason the lane has no inventory - printed once,
        // by whoever needs the answer.
        let (attempted, held) = if certifying {
            (attempt(preparer), Vec::new())
        } else {
            output::capture_errors(|| attempt(preparer))
        };
        let prepared = match attempted {
            Ok(p) => {
                if !held.is_empty() {
                    output::detail(&format!("prepare: sweep '{}': {}", sweep.label, held.join("\n")));
                }
                p
            }
            Err(e) => {
                if matches!(e, DevError::Interrupted) || crate::shutdown::is_shutdown_requested() {
                    stopped = true;
                    plan.incomplete.push(format!(
                        "preparation stopped at sweep '{}' before every lane was prepared",
                        sweep.label
                    ));
                } else {
                    plan.incomplete.push(format!("sweep '{}': {e}", sweep.label));
                }
                if certifying {
                    for f in &sweep.declared_filters {
                        ledger.record_unknown(f, &sweep.label);
                    }
                }
                let why = if held.is_empty() { e.to_string() } else { held.join(" - ") };
                record.unavailable = Some(format!("preparation failed: {why}"));
                if !held.is_empty() {
                    output::detail(&format!("prepare: sweep '{}': {}", sweep.label, held.join("\n")));
                }
                let message = e.to_string();
                if error.is_none() {
                    error = Some(e);
                }
                plan.lanes.push(record);
                // A stop leaves no record: the run is ending. Any other
                // failure outside a claim is kept, so the lane reports it
                // when it runs instead of preparing - and listing - again.
                lanes.push(if certifying || stopped {
                    None
                } else {
                    Some(PreparedLane::failed(PrepareFailure { held, message }))
                });
                continue;
            }
        };

        if let Some(reason) = &prepared.skipped {
            // Not run by this invocation: nothing expected, nothing to judge.
            record.skipped = Some(reason.clone());
            record.prepared = true;
            plan.lanes.push(record);
            lanes.push(Some(prepared));
            continue;
        }
        if let Some(reason) = &prepared.unavailable {
            // The lane runs, through cargo, with its streams unattributable:
            // no inventory, and the filters it declares cannot be judged.
            record.unavailable = Some(reason.clone());
            if certifying {
                for f in &sweep.declared_filters {
                    ledger.record_unknown(f, &sweep.label);
                }
            }
            plan.incomplete.push(format!("sweep '{}': no inventory - {reason}", sweep.label));
            plan.lanes.push(record);
            lanes.push(Some(prepared));
            continue;
        }

        // Each resolution of a non-doc lane needs its shape's universe, once.
        // Policy only: an inventory is the lane's own selection.
        let mut lane_ok = true;
        if certifying && kind != LaneKind::DocOnly {
            for r in &prepared.resolutions {
                let key = (shape.clone(), r.resolution.clone());
                if !universes.contains_key(&key) {
                    shape_order.push((shape.clone(), r.resolution.clone(), idx));
                    let u = match preparer.universe(sweep, r.resolution.as_deref(), &prepared.env) {
                        Ok(u) => Some(u),
                        Err(e) => {
                            plan.incomplete.push(format!("sweep '{}': universe: {e}", sweep.label));
                            if matches!(e, DevError::Interrupted) || crate::shutdown::is_shutdown_requested() {
                                stopped = true;
                            }
                            if error.is_none() {
                                error = Some(e);
                            }
                            None
                        }
                    };
                    universes.insert(key.clone(), u);
                }
                let Some(Some(universe)) = universes.get(&key) else {
                    lane_ok = false;
                    continue;
                };
                // A lane binary outside its shape's universe is a plan that
                // contradicts itself: the lane would run pairs policy never saw.
                for b in &r.binaries {
                    if !universe.binaries.iter().any(|(u, _, _)| *u == b.unit) {
                        lane_ok = false;
                        plan.incomplete.push(format!(
                            "sweep '{}': {} is not in its shape's universe",
                            sweep.label,
                            b.unit.id()
                        ));
                    }
                }
            }
        }

        // Filter liveness, judged against the names the LANE's binaries can
        // see - recorded here, decided once every lane has been (a profile
        // filter's scope is the profile).
        if certifying && kind != LaneKind::DocOnly && lane_ok {
            let mut candidates: Vec<(String, String)> = Vec::new();
            let mut ignored: BTreeSet<(String, String)> = BTreeSet::new();
            for r in &prepared.resolutions {
                if let Some(Some(universe)) = universes.get(&(shape.clone(), r.resolution.clone())) {
                    for b in &r.binaries {
                        for (u, all, ig) in &universe.binaries {
                            if *u == b.unit {
                                candidates.extend(all.iter().map(|t| (u.package.clone(), t.clone())));
                                ignored.extend(ig.iter().map(|t| (u.package.clone(), t.clone())));
                            }
                        }
                    }
                }
            }
            for (filter, live) in filter_liveness(sweep, &candidates, &ignored) {
                ledger.record(filter, live, &sweep.label);
            }
        } else if certifying && kind != LaneKind::DocOnly {
            for f in &sweep.declared_filters {
                ledger.record_unknown(f, &sweep.label);
            }
        }

        for r in &prepared.resolutions {
            for b in &r.binaries {
                for name in &b.selected {
                    let pair = PairId {
                        shape: shape.clone(),
                        resolution: r.resolution.clone(),
                        unit: b.unit.clone(),
                        test: name.clone(),
                    };
                    if prepared.include_ignored || !b.ignored.contains(name) {
                        record.executions.push(pair);
                    } else {
                        record.ignored_selected.push(pair);
                    }
                }
                for name in &b.benchmarks {
                    record.outside_claim.push(PairId {
                        shape: shape.clone(),
                        resolution: r.resolution.clone(),
                        unit: b.unit.clone(),
                        test: name.clone(),
                    });
                }
            }
        }
        record.artifacts = prepared.artifacts();
        record.unlisted = prepared
            .resolutions
            .iter()
            .flat_map(|r| r.binaries.iter())
            .filter_map(|b| b.unlisted.as_ref().map(|why| (b.unit.id(), why.clone())))
            .collect();
        for (id, why) in &record.unlisted {
            plan.incomplete.push(format!("sweep '{}': {id} cannot be enumerated - {why}", sweep.label));
        }
        record.include_ignored = prepared.include_ignored;
        if record.doc_carrier && kind == LaneKind::Serial {
            // From the selected targets' metadata, not from which harnesses
            // the build produced: a library harness neither implies doctests
            // (`doctest = false`) nor is implied by them (`test = false`).
            record.doc_streams_required = prepared.doc_obligation;
        }
        record.prepared = lane_ok;
        plan.lanes.push(record);
        lanes.push(Some(prepared));
    }

    for (shape, resolution, first) in shape_order {
        let members: Vec<&LaneRecord> = plan
            .lanes
            .iter()
            .filter(|l| l.shape == shape && l.kind != LaneKind::DocOnly)
            .collect();
        let universe = universes.get(&(shape.clone(), resolution.clone())).and_then(Option::as_ref);
        let sweep = &sweeps[first];
        plan.shapes.push(ShapeRecord {
            shape: shape.clone(),
            label: match &resolution {
                Some(pkg) => format!("{}/{pkg}", sweep.label),
                None => sweep.label.clone(),
            },
            resolution,
            curated: members.iter().all(|l| sweeps[l.lane].curated),
            complete: universe.is_some() && members.iter().all(|l| l.prepared),
            universe: universe
                .map(|u| {
                    u.binaries
                        .iter()
                        .flat_map(|(unit, all, _)| all.iter().map(move |t| (unit.clone(), t.clone())))
                        .collect()
                })
                .unwrap_or_default(),
            ignored: universe
                .map(|u| {
                    u.binaries
                        .iter()
                        .flat_map(|(unit, _, ig)| ig.iter().map(move |t| (unit.clone(), t.clone())))
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    plan.dead_filters = ledger
        .dead()
        .into_iter()
        .map(|d| DeadFilterRecord { sweeps: d.sweeps, origin: d.origin, label: d.label })
        .collect();
    plan.complete = plan.incomplete.is_empty() && plan.lanes.iter().all(|l| l.prepared || l.kind == LaneKind::DocOnly);
    ProfilePrep { plan, lanes, error }
}

#[cfg(test)]
mod prepare_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A preparer that answers from canned data, failing where told to.
    struct Fake {
        /// Sweep labels whose preparation is "stopped" as a watchdog stops it.
        stop_at: Option<String>,
        lanes_prepared: Vec<String>,
        support_fails: bool,
        support_artifacts: Vec<SupportArtifact>,
    }

    impl Fake {
        fn new(stop_at: Option<&str>) -> Self {
            Self {
                stop_at: stop_at.map(str::to_owned),
                lanes_prepared: Vec::new(),
                support_fails: false,
                support_artifacts: Vec::new(),
            }
        }
    }

    fn binary(target: &str) -> TestBinary {
        test_binary_for_tests("core", "test", target)
    }

    impl Preparer for Fake {
        fn support(&mut self, sweep: &ResolvedSweep) -> Result<Vec<SupportArtifact>, DevError> {
            if !sweep.build_packages.is_empty() {
                self.lanes_prepared.push(format!("support:{}", sweep.label));
            }
            if self.support_fails {
                return Err(DevError::Build(format!("support build failed for '{}'", sweep.label)));
            }
            Ok(self.support_artifacts.clone())
        }

        fn lane(&mut self, sweep: &ResolvedSweep) -> Result<PreparedLane, DevError> {
            if self.stop_at.as_deref() == Some(sweep.label.as_str()) {
                return Err(DevError::Interrupted);
            }
            self.lanes_prepared.push(sweep.label.clone());
            let b = binary("suite");
            let mut p = PreparedLane::bare(LaneEnv::default());
            p.resolutions.push(PreparedResolution {
                resolution: None,
                selection: Vec::new(),
                binaries: vec![PreparedBinary {
                    unit: BinaryUnit::of(&b),
                    binary: b,
                    hash: "h".into(),
                    unlisted: None,
                    selected: vec!["a".into(), "slow".into()],
                    ignored: ["slow".to_owned()].into_iter().collect(),
                    benchmarks: Vec::new(),
                }],
            });
            Ok(p)
        }

        fn universe(&mut self, _: &ResolvedSweep, _: Option<&str>, _: &LaneEnv) -> Result<Universe, DevError> {
            let b = binary("suite");
            Ok(Universe {
                binaries: vec![(
                    BinaryUnit::of(&b),
                    vec!["a".into(), "slow".into(), "b".into()],
                    ["slow".to_owned()].into_iter().collect(),
                )],
            })
        }
    }

    fn sweep(label: &str) -> ResolvedSweep {
        ResolvedSweep { label: label.into(), parallel_budget: Some(4), ..ResolvedSweep::default() }
    }

    #[test]
    fn a_prepared_profile_plans_executions_and_its_universe() {
        let mut fake = Fake::new(None);
        let prep = prepare_profile(&[sweep("one")], false, true, &mut fake);
        assert!(prep.plan.complete, "{:?}", prep.plan.incomplete);
        let lane = &prep.plan.lanes[0];
        let tests: Vec<&str> = lane.executions.iter().map(|p| p.test.as_str()).collect();
        assert_eq!(tests, vec!["a"], "the ignored name is selected, not executed");
        assert_eq!(lane.ignored_selected.len(), 1);
        assert_eq!(prep.plan.shapes.len(), 1);
        assert_eq!(prep.plan.shapes[0].universe.len(), 3);
        assert!(prep.plan.shapes[0].complete);
    }

    /// Validation case: a watchdog during preparation. The plan comes back -
    /// it is what the audit reads - but `complete = false`, with the stop on
    /// it, and nothing after the stop is prepared.
    #[test]
    fn a_stop_during_preparation_yields_an_incomplete_plan() {
        let mut fake = Fake::new(Some("two"));
        let prep = prepare_profile(&[sweep("one"), sweep("two"), sweep("three")], false, true, &mut fake);
        assert!(!prep.plan.complete);
        assert!(prep.plan.incomplete.iter().any(|m| m.contains("stopped")), "{:?}", prep.plan.incomplete);
        assert_eq!(fake.lanes_prepared, vec!["one"], "nothing is prepared after the stop");
        assert!(prep.plan.lanes[0].prepared);
        assert!(!prep.plan.lanes[1].prepared && !prep.plan.lanes[2].prepared);
        assert!(matches!(prep.error, Some(DevError::Interrupted)));
    }

    /// A lane binary missing from its shape's universe is a plan that
    /// contradicts itself, and is marked rather than trusted.
    #[test]
    fn a_lane_binary_outside_the_universe_is_a_marker() {
        struct Elsewhere;
        impl Preparer for Elsewhere {
            fn support(&mut self, _: &ResolvedSweep) -> Result<Vec<SupportArtifact>, DevError> {
                Ok(Vec::new())
            }
            fn lane(&mut self, s: &ResolvedSweep) -> Result<PreparedLane, DevError> {
                Fake::new(None).lane(s)
            }
            fn universe(&mut self, _: &ResolvedSweep, _: Option<&str>, _: &LaneEnv) -> Result<Universe, DevError> {
                Ok(Universe::default())
            }
        }
        let prep = prepare_profile(&[sweep("one")], false, true, &mut Elsewhere);
        assert!(!prep.plan.complete);
        assert!(!prep.plan.lanes[0].prepared);
    }

    /// A lane's declared support builds run before its listings - a listing
    /// whose static constructor reads a support binary must see the one the
    /// lane runs with - and a support build that fails is the lane's marker,
    /// with nothing listed after it.
    #[test]
    fn support_builds_run_before_the_lanes_listings() {
        let with_support = ResolvedSweep { build_packages: vec!["server".into()], ..sweep("one") };
        let mut fake = Fake::new(None);
        let prep = prepare_profile(std::slice::from_ref(&with_support), false, true, &mut fake);
        assert!(prep.plan.complete, "{:?}", prep.plan.incomplete);
        assert_eq!(fake.lanes_prepared, vec!["support:one", "one"]);

        let mut failing = Fake { support_fails: true, ..Fake::new(None) };
        let prep = prepare_profile(&[with_support], false, true, &mut failing);
        assert!(!prep.plan.complete);
        assert!(prep.plan.incomplete.iter().any(|m| m.contains("support build failed")), "{:?}", prep.plan.incomplete);
        assert_eq!(failing.lanes_prepared, vec!["support:one"], "nothing is listed past a failed support build");
    }

    /// libtest refuses `--ignored` beside `--include-ignored`, so the
    /// ignored-only listing of a lane that lifts `#[ignore]` must drop the
    /// latter - or the lane cannot be prepared at all.
    #[test]
    fn the_ignored_listing_never_carries_include_ignored() {
        let args = ignored_listing_args(&["tests::", "--skip", "slow", "--include-ignored"]);
        assert_eq!(args, vec!["tests::", "--skip", "slow", "--ignored"]);
        assert_eq!(ignored_listing_args(&[]), vec!["--ignored"]);
        // A forwarded `--ignored` is replaced, not doubled: getopts refuses a
        // repeated flag, which failed the listing and so the preparation.
        assert_eq!(ignored_listing_args(&["--ignored", "t::"]), vec!["t::", "--ignored"]);
    }

    /// Validation case: a forwarded ignored flag counts. `brokkr check -- --
    /// --ignored` lists ignored tests only, and they must stay in the
    /// expected executions - the sweep's own libtest args alone said
    /// "exclude" and dropped every one of them.
    #[test]
    fn forwarded_ignored_flags_reach_the_lane_s_ignored_mode() {
        let plain = ResolvedSweep::default();
        let forwarded = |args: &[&str]| args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(!lane_runs_ignored(&plain, &[]));
        assert!(lane_runs_ignored(&plain, &forwarded(&["--ignored"])));
        assert!(lane_runs_ignored(&plain, &forwarded(&["--include-ignored"])));
        let lifting = ResolvedSweep { libtest_args: forwarded(&["--include-ignored"]), ..ResolvedSweep::default() };
        assert!(lane_runs_ignored(&lifting, &[]));
        // `--skip`'s value is never read as a flag.
        assert!(!args_run_ignored(["--skip", "--ignored", "x"]));

        let mut p = PreparedLane::bare(LaneEnv::default());
        p.include_ignored = lane_runs_ignored(&plain, &forwarded(&["--ignored"]));
        assert!(p.include_ignored, "the lane executes the ignored tests it lists");
        let b = PreparedBinary {
            binary: binary("suite"),
            unit: BinaryUnit::of(&binary("suite")),
            hash: "h".into(),
            unlisted: None,
            selected: vec!["slow".into()],
            ignored: ["slow".to_owned()].into_iter().collect(),
            benchmarks: Vec::new(),
        };
        assert_eq!(b.executed(p.include_ignored).count(), 1);
    }

    /// Validation case: preparation runs on every partial check, so it must
    /// not execute a `harness = false` target to ask whether it lists. The
    /// executable here would leave a marker if it were run.
    #[test]
    fn a_harness_false_target_is_never_executed_to_list_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::test_scratch::scratch("prepare", "no_harness_listing");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"core\"\nversion = \"0.1.0\"\n\n[[test]]\nname = \"custom\"\nharness = false\n",
        )
        .unwrap();
        let marker = dir.join("RAN");
        let exe = dir.join("custom-1");
        std::fs::write(&exe, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = TestBinary {
            executable: exe.to_string_lossy().into_owned(),
            manifest_dir: dir.clone(),
            ..test_binary_for_tests("core", "test", "custom")
        };
        let runtime = DirectRuntime {
            packages: HashMap::new(),
            linked_paths: Vec::new(),
            index: BuildRuntimeIndex::default(),
            libdir: "/toolchain/lib".into(),
            cargo: "cargo".into(),
            config_env: Vec::new(),
        };
        let inputs = LaneInputs {
            project: None,
            project_root: &dir,
            state_root: &dir,
            target_dir: &dir,
            allow_flags: &[],
            commands: false,
            certifying: false,
        };
        let mut skipped = 0;
        let p = list_binary_or_unlisted(&inputs, &ResolvedSweep::default(), &target, &[], &[], &runtime, &mut skipped)
            .unwrap();
        assert!(!marker.exists(), "the harness = false executable was run");
        let why = p.unlisted.expect("kept as unlisted");
        assert!(why.contains("harness = false") && why.contains("not run"), "{why}");
        assert!(p.selected.is_empty() && !p.hash.is_empty(), "still in the inventory by identity");
    }

    /// A `harness = false` target whose executable leaves a marker if run,
    /// with the inputs a listing needs.
    fn marker_harness(scratch: &str) -> (std::path::PathBuf, std::path::PathBuf, TestBinary, DirectRuntime) {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::test_scratch::scratch("prepare", scratch);
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"core\"\nversion = \"0.1.0\"\n\n[[test]]\nname = \"custom\"\nharness = false\n",
        )
        .unwrap();
        let marker = dir.join("RAN");
        let exe = dir.join("custom-1");
        std::fs::write(&exe, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = TestBinary {
            executable: exe.to_string_lossy().into_owned(),
            manifest_dir: dir.clone(),
            ..test_binary_for_tests("core", "test", "custom")
        };
        let runtime = DirectRuntime {
            packages: HashMap::new(),
            linked_paths: Vec::new(),
            index: BuildRuntimeIndex::default(),
            libdir: "/toolchain/lib".into(),
            cargo: "cargo".into(),
            config_env: Vec::new(),
        };
        (dir, marker, target, runtime)
    }

    fn inputs_in<'a>(dir: &'a Path, certifying: bool) -> LaneInputs<'a> {
        LaneInputs {
            project: None,
            project_root: dir,
            state_root: dir,
            target_dir: dir,
            allow_flags: &[],
            commands: false,
            certifying,
        }
    }

    /// A `harness = false` target that answers `--list` the way libtest-mimic
    /// does (the terse listing, an empty one for `--ignored`), counting every
    /// invocation in `CALLS`.
    fn listing_harness(scratch: &str) -> (std::path::PathBuf, std::path::PathBuf, TestBinary, DirectRuntime) {
        use std::os::unix::fs::PermissionsExt as _;
        let (dir, _marker, mut target, runtime) = marker_harness(scratch);
        let calls = dir.join("CALLS");
        let exe = dir.join("custom-1");
        std::fs::write(
            &exe,
            format!(
                "#!/bin/sh\necho x >> {}\ncase \"$*\" in\n  *--ignored*) printf '\\n0 tests, 0 benchmarks\\n' ;;\n  \
                 *) printf 'alpha: test\\n\\n1 test, 0 benchmarks\\n' ;;\nesac\n",
                calls.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        target.executable = exe.to_string_lossy().into_owned();
        (dir, calls, target, runtime)
    }

    /// The parallel and isolated lanes' preparation (the listing half of
    /// `prepare_direct`) lists a `harness = false` target exactly as before
    /// preparation existed - a custom harness with a valid listing is
    /// enumerated and its tests are the lane's selection - on a partial run
    /// and a certifying one.
    #[test]
    fn direct_preparation_lists_a_harness_false_target_with_a_valid_listing() {
        let (dir, _calls, target, runtime) = listing_harness("direct_valid_harness_listing");
        for certifying in [false, true] {
            let mut prepared = PreparedLane::bare(LaneEnv::default());
            let built = vec![(None, vec!["-p".to_owned(), "core".to_owned()], vec![target.clone()])];
            list_direct_binaries(
                &inputs_in(&dir, certifying),
                &ResolvedSweep { label: "par".into(), ..ResolvedSweep::default() },
                built,
                &mut prepared,
                &[],
                &[],
                &runtime,
            )
            .unwrap();
            let b = &prepared.resolutions[0].binaries[0];
            assert_eq!(b.selected, vec!["alpha".to_owned()], "certifying: {certifying}");
            assert!(b.unlisted.is_none() && b.executed(false).count() == 1);
        }
    }

    /// A custom harness that does not answer `--list` with a libtest listing
    /// fails the direct lane's preparation cleanly, as enumeration always has.
    #[test]
    fn direct_preparation_fails_cleanly_on_a_harness_false_target_without_a_listing() {
        let (dir, marker, target, runtime) = marker_harness("direct_invalid_harness_listing");
        let mut prepared = PreparedLane::bare(LaneEnv::default());
        let built = vec![(None, vec!["-p".to_owned(), "core".to_owned()], vec![target])];
        let err = list_direct_binaries(
            &inputs_in(&dir, false),
            &ResolvedSweep { label: "par".into(), ..ResolvedSweep::default() },
            built,
            &mut prepared,
            &[],
            &[],
            &runtime,
        )
        .unwrap_err()
        .to_string();
        assert!(marker.exists(), "it was asked, as before");
        assert!(err.contains("sweep 'par' could not be prepared"), "{err}");
        assert!(prepared.resolutions.is_empty());
    }

    /// A certifying serial lane lists a custom harness like any binary.
    #[test]
    fn a_certifying_serial_lane_lists_a_harness_false_target() {
        let (dir, _calls, target, runtime) = listing_harness("serial_certifying_harness_listing");
        let mut skipped = 0;
        let p = list_binary_or_unlisted(
            &inputs_in(&dir, true),
            &ResolvedSweep { label: "ser".into(), ..ResolvedSweep::default() },
            &target,
            &[],
            &[],
            &runtime,
            &mut skipped,
        )
        .unwrap();
        assert_eq!(p.selected, vec!["alpha".to_owned()]);
        assert!(p.unlisted.is_none());
    }

    /// The certifying universe lists every binary of the shape, a custom
    /// harness included.
    #[test]
    fn the_universe_lists_a_harness_false_target() {
        let (dir, _calls, target, runtime) = listing_harness("universe_harness_listing");
        let u = universe_from_binaries(
            &inputs_in(&dir, true),
            &ResolvedSweep { label: "full".into(), ..ResolvedSweep::default() },
            std::slice::from_ref(&target),
            &[],
            &runtime,
        )
        .unwrap();
        assert_eq!(u.binaries[0].1, vec!["alpha".to_owned()]);
    }

    /// A lane whose up-front preparation failed outside a claim is not
    /// prepared a second time when it runs: that would execute every binary's
    /// `--list` again. The recorded failure is reported instead, and the
    /// preparer is not consulted.
    #[test]
    fn a_failed_preparation_is_not_retried_when_the_lane_runs() {
        struct Failing(usize);
        impl Preparer for Failing {
            fn support(&mut self, _: &ResolvedSweep) -> Result<Vec<SupportArtifact>, DevError> {
                Ok(Vec::new())
            }
            fn lane(&mut self, _: &ResolvedSweep) -> Result<PreparedLane, DevError> {
                self.0 += 1;
                output::error("failing command: ./custom --list");
                Err(DevError::Reported("sweep 'one' could not be prepared".into()))
            }
            fn universe(&mut self, _: &ResolvedSweep, _: Option<&str>, _: &LaneEnv) -> Result<Universe, DevError> {
                Ok(Universe::default())
            }
        }
        let mut failing = Failing(0);
        let sweep = sweep("one");
        let prep = prepare_profile(std::slice::from_ref(&sweep), false, false, &mut failing);
        assert_eq!(failing.0, 1);
        let record = prep.lanes[0].as_ref().expect("the failure is recorded on the lane");
        assert!(record.prepare_failed.as_ref().unwrap().held.iter().any(|l| l.contains("./custom --list")));

        let dir = crate::test_scratch::scratch("prepare", "no_retry_after_failed_preparation");
        let tap = LaneTap::new(0);
        let err = run_test_lane(
            &LaneArgs {
                project: None,
                project_root: &dir,
                state_root: &dir,
                target_dir: &dir,
                sweep: &sweep,
                scope: &[],
                extra_args: &[],
                allow_flags: &[],
                doctests: false,
                multi: false,
                commands: false,
                certifying: false,
            },
            Some(record),
            &tap,
            None,
        )
        .unwrap_err();
        // Preparing again would have gone to cargo in the empty scratch dir and
        // failed differently; this is the recorded error.
        assert!(matches!(err, DevError::Reported(ref m) if m == "sweep 'one' could not be prepared"), "{err}");
    }

    fn written(dir: &Path, name: &str, body: &[u8]) -> TestBinary {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        TestBinary { executable: path.to_string_lossy().into_owned(), ..test_binary_for_tests("core", "test", name) }
    }

    fn prepared_from(dir: &Path, binaries: &[(&str, &[u8])], resolutions: &[Option<&str>]) -> PreparedLane {
        let mut p = PreparedLane::bare(LaneEnv::default());
        for (r, (name, body)) in resolutions.iter().zip(binaries) {
            let b = written(dir, name, body);
            p.resolutions.push(PreparedResolution {
                resolution: r.map(str::to_owned),
                selection: Vec::new(),
                binaries: vec![PreparedBinary {
                    unit: BinaryUnit::of(&b),
                    hash: hash_file(Path::new(&b.executable)).unwrap(),
                    unlisted: None,
                    binary: b,
                    selected: vec!["t".into()],
                    ignored: BTreeSet::new(),
                    benchmarks: Vec::new(),
                }],
            });
        }
        p
    }

    /// Verification hashes after EVERY resolution is rebuilt: a later
    /// resolution's build that rewrites an earlier one's binary in place is
    /// drift, where hashing each resolution right after its own build
    /// certified the file the next build then replaced.
    #[test]
    fn verification_hashes_after_every_resolution_is_built() {
        let dir = crate::test_scratch::scratch("prepare", "verify_after_all_builds");
        let p = prepared_from(&dir, &[("a", b"a planned"), ("b", b"b planned")], &[Some("pa"), Some("pb")]);
        let a_path = dir.join("a");
        let mut builds = 0;
        let s = sweep("one");
        let err = verify_lane_with(&s, &p, |r| {
            builds += 1;
            // The second resolution's build clobbers the first's binary.
            if r.resolution.as_deref() == Some("pb") {
                std::fs::write(&a_path, b"another shape's a").unwrap();
            }
            let b = r.binaries[0].binary.clone();
            Ok(Some((vec![b], BuildRuntimeIndex::default())))
        }, &[])
        .unwrap_err()
        .to_string();
        assert_eq!(builds, 2);
        assert!(err.contains("different content"), "{err}");

        // Untouched, the same plan verifies.
        let dir = crate::test_scratch::scratch("prepare", "verify_untouched");
        let p = prepared_from(&dir, &[("a", b"a planned")], &[None]);
        verify_lane_with(&s, &p, |r| Ok(Some((vec![r.binaries[0].binary.clone()], BuildRuntimeIndex::default()))), &[])
            .unwrap();
    }

    /// A `build_packages` support executable (outside the test selection,
    /// reached through `BROKKR_TEST_BIN_DIR`, named by no test build's stream)
    /// is part of the plan: the profile hashes it after the lane is built,
    /// and a lane whose support binary was rebuilt since does not run.
    #[test]
    fn support_executables_are_planned_and_verified() {
        let dir = crate::test_scratch::scratch("prepare", "support_verified");
        let server = dir.join("server");
        std::fs::write(&server, b"planned server").unwrap();
        let support = vec![SupportArtifact {
            package: "server".into(),
            target: "server".into(),
            executable: server.to_string_lossy().into_owned(),
        }];
        let with_support = ResolvedSweep { build_packages: vec!["server".into()], ..sweep("one") };
        let mut fake = Fake { support_artifacts: support.clone(), ..Fake::new(None) };
        let prep = prepare_profile(std::slice::from_ref(&with_support), false, true, &mut fake);
        let lane = prep.lanes[0].as_ref().unwrap();
        assert_eq!(lane.support_fingerprint.len(), 1, "{:?}", lane.support_fingerprint);

        let mut p = prepared_from(&dir, &[("a", b"a planned")], &[None]);
        p.support_fingerprint = lane.support_fingerprint.clone();
        let build = |r: &PreparedResolution| Ok(Some((vec![r.binaries[0].binary.clone()], BuildRuntimeIndex::default())));
        verify_lane_with(&with_support, &p, build, &support).unwrap();
        std::fs::write(&server, b"another lane's server").unwrap();
        let err = verify_lane_with(&with_support, &p, build, &support).unwrap_err().to_string();
        assert!(err.contains("support build"), "{err}");
        // A support build that now produces nothing is drift too.
        let err = verify_lane_with(&with_support, &p, build, &[]).unwrap_err().to_string();
        assert!(err.contains("support build"), "{err}");
    }

    /// The launch envelope is verified with the executables: a support bin
    /// behind `CARGO_BIN_EXE_<name>` rebuilt with other content since the plan
    /// is drift, though every test executable is unchanged.
    #[test]
    fn verification_covers_the_runtime_index() {
        let dir = crate::test_scratch::scratch("prepare", "verify_runtime_index");
        let mut p = prepared_from(&dir, &[("a", b"a planned")], &[None]);
        let support = dir.join("server");
        std::fs::write(&support, b"planned server").unwrap();
        let index = || {
            let mut i = BuildRuntimeIndex::default();
            i.bin_exes.insert(
                "pkg-id".into(),
                vec![("server".into(), support.to_string_lossy().into_owned())],
            );
            i
        };
        p.runtime_fingerprint = Some(index().fingerprint().unwrap());
        let s = sweep("one");
        let build = |r: &PreparedResolution| Ok(Some((vec![r.binaries[0].binary.clone()], index())));
        verify_lane_with(&s, &p, build, &[]).unwrap();
        std::fs::write(&support, b"another shape's server").unwrap();
        let err = verify_lane_with(&s, &p, build, &[]).unwrap_err().to_string();
        assert!(err.contains("launch envelope"), "{err}");
    }

    /// The doctest obligation comes from the selected targets' metadata. A
    /// library with `test = true, doctest = false` has a harness and no
    /// doctests - requiring a rustdoc stream there failed a healthy carrier;
    /// one with `test = false, doctest = true` has doctests and no harness -
    /// a carrier presenting no rustdoc stream there counted as completed.
    #[test]
    fn the_doctest_obligation_follows_cargo_metadata_not_harnesses() {
        let lib = |name: &str, test: bool, doctest: bool| {
            serde_json::json!({
                "id": format!("path+file:///x/{name}#{name}@0.1.0"),
                "name": name,
                "targets": [{
                    "name": name, "kind": ["lib"], "crate_types": ["lib"],
                    "test": test, "doctest": doctest,
                }],
            })
        };
        let meta = serde_json::json!({
            "packages": [
                lib("harness_only", true, false),
                lib("doc_only", false, true),
                {
                    "id": "path+file:///x/cdy#cdy@0.1.0", "name": "cdy",
                    "targets": [{ "name": "cdy", "kind": ["cdylib"], "crate_types": ["cdylib"], "doctest": true }],
                },
            ],
            "workspace_members": [
                "path+file:///x/harness_only#harness_only@0.1.0",
                "path+file:///x/doc_only#doc_only@0.1.0",
                "path+file:///x/cdy#cdy@0.1.0",
            ],
            "workspace_default_members": ["path+file:///x/harness_only#harness_only@0.1.0"],
        });
        let facts = DocFacts::from_metadata(&meta);
        let only = |pkg: &str| ResolvedSweep { packages: vec![pkg.into()], ..ResolvedSweep::default() };
        assert!(!facts.obliges(&only("harness_only"), &[]), "a library harness without doctests obliges nothing");
        assert!(facts.obliges(&only("doc_only"), &[]), "doctests without a library harness oblige a stream");
        assert!(!facts.obliges(&only("cdy"), &[]), "rustdoc tests no cdylib");
        // The default selection is the default members; an exclusion list is
        // the whole workspace less it; a resolution's scope replaces both.
        assert!(!facts.obliges(&ResolvedSweep::default(), &[]));
        let excl = ResolvedSweep { test_exclude_packages: vec!["cdy".into()], ..ResolvedSweep::default() };
        assert!(facts.obliges(&excl, &[]));
        assert!(facts.obliges(&ResolvedSweep::default(), &["doc_only"]));
    }

    /// Validation case: cargo-mediated parallelism on the serial lane is
    /// refused under a complete claim, pointing at the attributable lane.
    #[test]
    fn cargo_mediated_parallelism_is_refused_under_complete() {
        let inputs = LaneInputs {
            project: None,
            project_root: Path::new("/nonexistent"),
            state_root: Path::new("/nonexistent"),
            target_dir: Path::new("/nonexistent/target"),
            allow_flags: &[],
            commands: false,
            certifying: true,
        };
        let parallel_serial = ResolvedSweep {
            label: "threaded".into(),
            test_threads: Some(0),
            ..ResolvedSweep::default()
        };
        let err = refuse_unattributable_serial(&inputs, &parallel_serial, &LaneEnv::default(), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("parallel = { budget = N }"), "{err}");
        let serial = ResolvedSweep { label: "serial".into(), test_threads: Some(1), ..ResolvedSweep::default() };
        // The serial sweep passes the parallelism check; whether the shim can
        // run on this host is the second check, decided by the environment.
        let second = refuse_unattributable_serial(&inputs, &serial, &LaneEnv::default(), &[]);
        assert!(second.as_ref().is_ok() || second.unwrap_err().to_string().contains("cannot isolate"));
    }

    /// A lane that cannot be attributed to binaries: inventory unavailable.
    struct Blind;
    impl Preparer for Blind {
        fn support(&mut self, _: &ResolvedSweep) -> Result<Vec<SupportArtifact>, DevError> {
            Ok(Vec::new())
        }
        fn lane(&mut self, _: &ResolvedSweep) -> Result<PreparedLane, DevError> {
            Ok(PreparedLane::without_inventory(
                LaneEnv::default(),
                "cargo-mediated parallelism shares one stream".into(),
            ))
        }
        fn universe(&mut self, _: &ResolvedSweep, _: Option<&str>, _: &LaneEnv) -> Result<Universe, DevError> {
            Err(DevError::Build("a non-certifying run enumerates no universe".into()))
        }
    }

    /// Validation case: every run prepares its inventory - executions,
    /// artifacts with their content hashes - without the policy universe,
    /// which only a certifying claim enumerates (the `Blind` preparer's
    /// universe would fail the test if it were asked for).
    #[test]
    fn a_partial_run_prepares_an_inventory_without_a_policy_universe() {
        struct NoUniverse(Fake);
        impl Preparer for NoUniverse {
            fn support(&mut self, s: &ResolvedSweep) -> Result<Vec<SupportArtifact>, DevError> {
                self.0.support(s)
            }
            fn lane(&mut self, s: &ResolvedSweep) -> Result<PreparedLane, DevError> {
                self.0.lane(s)
            }
            fn universe(&mut self, _: &ResolvedSweep, _: Option<&str>, _: &LaneEnv) -> Result<Universe, DevError> {
                Err(DevError::Build("asked for a universe".into()))
            }
        }
        let serial = ResolvedSweep { label: "serial".into(), ..ResolvedSweep::default() };
        let prep = prepare_profile(&[serial], false, false, &mut NoUniverse(Fake::new(None)));
        assert!(prep.error.is_none(), "{:?}", prep.error);
        assert!(prep.plan.complete, "{:?}", prep.plan.incomplete);
        assert!(!prep.plan.certifying);
        assert!(prep.plan.shapes.is_empty() && prep.plan.dead_filters.is_empty(), "policy is for certifying runs");
        let lane = &prep.plan.lanes[0];
        assert!(lane.prepared);
        assert_eq!(lane.executions.len(), 1, "the ignored name is selected, not executed");
        assert_eq!(lane.artifacts.len(), 1);
        assert_eq!(lane.artifacts[0].hash, "h", "the artifact's identity is recorded on every run");
    }

    /// Validation case: a lane with no attribution reports its inventory
    /// unavailable - with the reason and no executions - instead of
    /// inventing names, and the run is not refused for it.
    #[test]
    fn an_unattributable_lane_has_no_inventory_and_is_not_refused() {
        let threaded = ResolvedSweep { label: "threaded".into(), test_threads: Some(0), ..ResolvedSweep::default() };
        let prep = prepare_profile(&[threaded], false, false, &mut Blind);
        assert!(prep.error.is_none(), "unavailable by design is not a failure: {:?}", prep.error);
        let lane = &prep.plan.lanes[0];
        assert!(!lane.prepared);
        assert!(lane.unavailable.as_deref().unwrap().contains("one stream"));
        assert!(lane.executions.is_empty() && lane.artifacts.is_empty());
        assert!(!prep.plan.complete, "a plan with a blind lane is not whole");
        assert!(prep.lanes[0].is_some(), "the lane still runs, through cargo");
    }

    /// Outside a certifying claim a lane that fails to prepare is a lane with
    /// no inventory, its error held back for the lane to print itself; under
    /// a claim it is the failure it always was.
    #[test]
    fn a_preparation_failure_degrades_outside_a_claim_and_fails_inside_one() {
        struct Failing;
        impl Preparer for Failing {
            fn support(&mut self, _: &ResolvedSweep) -> Result<Vec<SupportArtifact>, DevError> {
                Ok(Vec::new())
            }
            fn lane(&mut self, _: &ResolvedSweep) -> Result<PreparedLane, DevError> {
                output::error("failing command: cargo test --no-run");
                Err(DevError::Reported("sweep 'one' could not be prepared".into()))
            }
            fn universe(&mut self, _: &ResolvedSweep, _: Option<&str>, _: &LaneEnv) -> Result<Universe, DevError> {
                Ok(Universe::default())
            }
        }
        let partial = prepare_profile(&[sweep("one")], false, false, &mut Failing);
        let lane = &partial.plan.lanes[0];
        assert!(lane.unavailable.as_deref().unwrap().contains("failing command: cargo test --no-run"), "{:?}", lane.unavailable);
        assert!(
            partial.lanes[0].as_ref().is_some_and(|l| l.prepare_failed.is_some()),
            "the failure is recorded for the lane to report, not prepared again"
        );
        assert!(!partial.plan.complete);
        let certifying = prepare_profile(&[sweep("one")], false, true, &mut Failing);
        assert!(certifying.error.is_some());
    }

    /// A binary that answers no libtest listing is kept with the reason, not
    /// at the cost of the lane's whole inventory.
    #[test]
    fn an_unlisted_binary_is_recorded_on_the_lane() {
        struct Custom;
        impl Preparer for Custom {
            fn support(&mut self, _: &ResolvedSweep) -> Result<Vec<SupportArtifact>, DevError> {
                Ok(Vec::new())
            }
            fn lane(&mut self, s: &ResolvedSweep) -> Result<PreparedLane, DevError> {
                let mut p = Fake::new(None).lane(s)?;
                let b = test_binary_for_tests("core", "test", "custom");
                p.resolutions[0].binaries.push(PreparedBinary {
                    unit: BinaryUnit::of(&b),
                    binary: b,
                    hash: "c".into(),
                    unlisted: Some("did not produce a complete libtest listing".into()),
                    selected: Vec::new(),
                    ignored: BTreeSet::new(),
                    benchmarks: Vec::new(),
                });
                Ok(p)
            }
            fn universe(&mut self, _: &ResolvedSweep, _: Option<&str>, _: &LaneEnv) -> Result<Universe, DevError> {
                Ok(Universe::default())
            }
        }
        let prep = prepare_profile(&[sweep("one")], false, false, &mut Custom);
        let lane = &prep.plan.lanes[0];
        assert_eq!(lane.unlisted.len(), 1);
        assert_eq!(lane.unlisted[0].0, "core::custom");
        assert_eq!(lane.artifacts.len(), 2, "the unlisted binary's identity is still recorded");
        assert!(lane.executions.iter().all(|p| p.unit.target != "custom"));
        assert!(!prep.plan.complete);
    }
}
