// The `prepare` phase: the whole profile planned before any test runs.
//
// Under `certifies = "complete"` every active lane is prepared up front - its
// shape built (the same `cargo test --no-run` the lane itself uses), every
// binary listed under the launch envelope execution will use, its selection
// and the executions it implies recorded - together with each shape
// resolution's universe, the exclusion ledger's raw material and a content
// hash of every test executable. The result is the plan: persisted before the
// first test executes, and the only thing the coverage audit reads besides
// the journal.
//
// Preparing up front is what separates the two claims the audit used to mix.
// Policy coverage is a property of what the profile SELECTS, so it must not
// depend on how far the test phase got: a lane a fail-fast never reached
// still selects its pairs, and its executions surface as unobserved rather
// than its pairs as orphaned. And it is what makes the audit possible after a
// watchdog kill at all - the old audit enumerated after the test phase, which
// the raised shutdown flag forbids.
//
// The lanes then CONSUME the prepared selection rather than re-enumerating.
// Outside a complete profile the same preparation runs per lane, just before
// it executes, for the lanes that need a selection to execute (parallel,
// isolated, nextest): one selection code path, whether or not anything is
// being certified.

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
    /// Under a complete profile: hash artifacts, refuse what cannot be
    /// attributed.
    pub(crate) complete: bool,
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
    /// Content hash, under a complete profile; empty otherwise.
    pub(crate) hash: String,
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
    /// The selection the lane's prebuild ran with, replayed to verify the
    /// artifacts before the lane executes.
    pub(crate) selection: Vec<String>,
    pub(crate) binaries: Vec<PreparedBinary>,
}

/// Everything a lane needs to execute without enumerating anything.
pub(crate) struct PreparedLane {
    pub(crate) env: LaneEnv,
    pub(crate) include_ignored: bool,
    /// Names a package-qualified skip removed, for the lane's report.
    pub(crate) pkg_skipped: usize,
    pub(crate) resolutions: Vec<PreparedResolution>,
    /// Target selectors the binaries were narrowed with after the prebuild.
    pub(crate) target_filters: Vec<String>,
    /// The direct-execution lanes' launch envelope.
    pub(crate) runtime: Option<DirectRuntime>,
    pub(crate) nextest: Option<NextestPrepared>,
    /// Forwarded cargo args (outside a complete profile), for repro lines.
    pub(crate) cargo_extra: Vec<String>,
    /// Forwarded libtest args, appended to every direct execution.
    pub(crate) libtest_extra: Vec<String>,
    /// Test binaries the prebuild produced before any narrowing.
    pub(crate) enumerated: usize,
    /// Prepared in the `prepare` phase: verify the artifacts again before
    /// running, since other lanes have built since.
    pub(crate) verify: bool,
    /// Under a complete profile, on the lanes that execute binaries
    /// directly or through the strict shim: the runtime index's fingerprint
    /// ([`BuildRuntimeIndex::fingerprint`]) the envelope was built from. The
    /// lane's re-verification compares a fresh one, so a launch envelope
    /// built from facts that no longer hold is caught, not used.
    pub(crate) runtime_fingerprint: Option<Vec<String>>,
    /// Under a complete profile: the executables the lane's `build_packages`
    /// pre-builds produced, by package, target, path and content
    /// ([`support_fingerprint`]), hashed after the lane's own build. A
    /// support package outside the test selection reaches the tests through
    /// `BROKKR_TEST_BIN_DIR`, which no test build's artifact stream names - so
    /// [`Self::runtime_fingerprint`] cannot see it, and without this a support
    /// binary another lane rebuilt since the plan ran unverified.
    pub(crate) support_fingerprint: Vec<String>,
    /// A serial lane's `cargo test` selection reaches a member with a
    /// doctested target ([`DocFacts::obliges`]), so - where the lane carries
    /// doctests - it must present a rustdoc stream to have completed.
    pub(crate) doc_obligation: bool,
}

impl PreparedLane {
    fn bare(env: LaneEnv) -> Self {
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
        }
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

/// The argv of a lane's ignored-only listing: its filter args with
/// `--ignored` added and `--include-ignored` taken out. libtest's `--list`
/// includes `#[ignore]`d names whatever the flags, so the ignored subset
/// comes from `--ignored`, which lists only them - and libtest refuses
/// `--ignored` beside `--include-ignored` outright, so a lane that lifts
/// `#[ignore]` would fail this listing, and with it its preparation.
fn ignored_listing_args<'a>(filter_args: &[&'a str]) -> Vec<&'a str> {
    let mut out: Vec<&str> = filter_args.iter().copied().filter(|a| *a != "--include-ignored").collect();
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
    let hash = if inputs.complete {
        hash_file(Path::new(&binary.executable))
            .map_err(|e| DevError::Build(format!("could not hash {}: {e}", binary.executable)))?
    } else {
        String::new()
    };
    Ok(Some(PreparedBinary {
        unit: BinaryUnit::of(binary),
        binary: binary.clone(),
        hash,
        selected,
        ignored: ignored.tests.into_iter().collect(),
        benchmarks: listed.benchmarks,
    }))
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
            if inputs.complete {
                refuse_unattributable_serial(inputs, sweep, &env, extra_args)?;
            }
            PreparedLane::bare(env)
        }
        LaneKind::Parallel | LaneKind::Isolated => prepare_direct(inputs, sweep, scope, extra_args, &env, kind)?,
        LaneKind::Serial => prepare_serial(inputs, sweep, scope, extra_args, &env)?,
    };
    prepared.verify = inputs.complete;
    Ok(prepared)
}

/// Refuse, under a complete profile, a serial lane whose harnesses cannot be
/// attributed: certification needs every harness on its own stream, and two
/// shapes cannot give that.
///
/// - Cargo-mediated parallelism (`test_threads` 0 or above 1) runs many tests
///   on one stream with no per-harness isolation; the parallel-binaries lane
///   (`parallel = { budget = N }`) runs them concurrently AND attributably.
/// - A shape where the shim cannot be installed (a configured runner, a cross
///   target, a configured rustdoc - `serial_shim_fallback`) leaves every
///   harness on cargo's one stream.
fn refuse_unattributable_serial(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    env: &LaneEnv,
    extra_args: &[String],
) -> Result<(), DevError> {
    if !sweep.doc_only && matches!(sweep.test_threads, Some(n) if n != 1) {
        return Err(DevError::Config(format!(
            "sweep '{}' runs its tests in parallel through cargo (`test_threads` other than 1), \
             which shares one output stream across tests and harnesses, so its executions cannot \
             be attributed under a \"complete\" claim. Use `parallel = {{ budget = N }}` on the \
             [[check]] entry instead - the parallel-binaries lane runs the binaries concurrently \
             with each one on its own stream.",
            sweep.label
        )));
    }
    let (cargo_extra, _) = split_extra_args(extra_args);
    if let Some(reason) = serial_shim_fallback(inputs.project_root, cargo_extra, &env.project_env, &sweep.env) {
        return Err(DevError::Config(format!(
            "sweep '{}' cannot isolate its test harnesses ({reason}), so its executions cannot be \
             attributed and a \"complete\" claim cannot be certified. Remove the override, or run \
             this lane under a profile that certifies nothing.",
            sweep.label
        )));
    }
    Ok(())
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

/// The serial lane's preparation (complete profiles only - outside one it
/// executes without a plan): the same build `cargo test` will do, listed.
fn prepare_serial(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    scope: &[&str],
    extra_args: &[String],
    env: &LaneEnv,
) -> Result<PreparedLane, DevError> {
    refuse_unattributable_serial(inputs, sweep, env, extra_args)?;
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
    let fingerprint = inputs.complete.then(|| index.fingerprint()).transpose()?;
    let runtime = DirectRuntime::load(inputs.project_root, &env_refs, index)?;
    let filter_args = lane_filter_args(sweep, libtest_extra);
    let mut prepared = PreparedLane::bare(env.clone());
    prepared.runtime_fingerprint = fingerprint;
    prepared.doc_obligation = doc_obligation;
    prepared.include_ignored = sweep.libtest_args.iter().any(|a| a == "--include-ignored");
    prepared.target_filters = sweep.cargo_test_filters.clone();
    for (resolution, selection, binaries) in built {
        prepared.enumerated += binaries.len();
        let mut out = Vec::new();
        for b in filter_binaries(&binaries, &prepared.target_filters) {
            let Some(p) = list_binary(inputs, sweep, b, &filter_args, &env_refs, &runtime, &mut prepared.pkg_skipped)?
            else {
                return Err(not_prepared(sweep));
            };
            out.push(p);
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
    let fingerprint = inputs.complete.then(|| index.fingerprint()).transpose()?;
    let runtime = DirectRuntime::load(inputs.project_root, &env_refs, index)?;
    let mut prepared = PreparedLane::bare(env.clone());
    prepared.runtime_fingerprint = fingerprint;
    prepared.include_ignored = sweep.libtest_args.iter().any(|a| a == "--include-ignored");
    // The sweep's own `--test` filters UNION with any the caller supplied,
    // matching cargo's semantics for repeated selection flags. Under package
    // mode the sweep's filters cannot ride the per-package prebuild (cargo
    // refuses a target a package lacks), so this is where they narrow.
    prepared.target_filters = sweep.cargo_test_filters.clone();
    prepared.target_filters.extend(extra_selectors.iter().cloned());
    prepared.cargo_extra = cargo_extra;
    prepared.libtest_extra = libtest_extra.to_vec();
    let filter_args = lane_filter_args(sweep, libtest_extra);
    for (resolution, selection, binaries) in built {
        prepared.enumerated += binaries.len();
        let mut out = Vec::new();
        for b in filter_binaries(&binaries, &prepared.target_filters) {
            let Some(p) = list_binary(inputs, sweep, b, &filter_args, &env_refs, &runtime, &mut prepared.pkg_skipped)?
            else {
                return Err(not_prepared(sweep));
            };
            out.push(p);
        }
        prepared.resolutions.push(PreparedResolution { resolution, selection, binaries: out });
    }
    prepared.runtime = Some(runtime);
    Ok(prepared)
}

/// Build a lane's shape again and prove what it is about to run is what was
/// planned. A no-op build normally (and the rebuild re-uplifts the support
/// binaries a later lane's build may have replaced); a difference is a hard
/// error for the lane, never a silent re-plan - a plan that follows the
/// artifacts around certifies whatever happens to be on disk.
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
pub(crate) fn verify_lane_artifacts(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    prepared: &PreparedLane,
    support: &[SupportArtifact],
) -> Result<(), DevError> {
    let env_refs = prepared.env.refs();
    verify_lane_with(
        sweep,
        prepared,
        |r| test_binaries_with_runtime(inputs.project_root, &r.selection, &env_refs, inputs.commands),
        support,
    )
}

/// [`verify_lane_artifacts`] over a given build step.
fn verify_lane_with(
    sweep: &ResolvedSweep,
    prepared: &PreparedLane,
    mut build: impl FnMut(&PreparedResolution) -> Result<Option<(Vec<TestBinary>, BuildRuntimeIndex)>, DevError>,
    support: &[SupportArtifact],
) -> Result<(), DevError> {
    // Every build first, then every hash: hashing between builds would
    // certify a file the next resolution's build then rewrote.
    let mut rebuilt: Vec<(Option<String>, Vec<TestBinary>)> = Vec::new();
    let mut index = BuildRuntimeIndex::default();
    for r in &prepared.resolutions {
        let Some((binaries, idx)) = build(r)? else {
            return Err(not_prepared(sweep));
        };
        index.merge(idx);
        rebuilt.push((r.resolution.clone(), binaries));
    }
    let mut current: Vec<(Option<String>, String, String)> = Vec::new();
    for (resolution, binaries) in &rebuilt {
        for b in filter_binaries(binaries, &prepared.target_filters) {
            let hash = hash_file(Path::new(&b.executable))
                .map_err(|e| DevError::Build(format!("could not hash {}: {e}", b.executable)))?;
            current.push((resolution.clone(), b.executable.clone(), hash));
        }
    }
    let mut drift = artifact_drift(&prepared.artifacts(), &current);
    if let Some(planned) = &prepared.runtime_fingerprint {
        let now = index.fingerprint()?;
        for line in planned.iter().filter(|l| !now.contains(l)) {
            drift.push(format!("the launch envelope's `{line}` no longer holds"));
        }
        for line in now.iter().filter(|l| !planned.contains(l)) {
            drift.push(format!("the launch envelope gained `{line}`"));
        }
    }
    let support_now = support_fingerprint(support)?;
    for line in prepared.support_fingerprint.iter().filter(|l| !support_now.contains(l)) {
        drift.push(format!("the support build's `{line}` no longer holds"));
    }
    for line in support_now.iter().filter(|l| !prepared.support_fingerprint.contains(l)) {
        drift.push(format!("the support build gained `{line}`"));
    }
    if drift.is_empty() {
        return Ok(());
    }
    Err(DevError::Build(format!(
        "sweep '{}': artifacts changed since the plan - {}. The lane would run binaries the plan \
         never enumerated, so it does not run at all.",
        sweep.label,
        drift.join("; ")
    )))
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
    let mut out = Universe::default();
    for b in &binaries {
        let Some(all) = binary_list(b, inputs.project_root, &["--include-ignored"], &env_refs, &runtime)? else {
            return Err(DevError::Reported(format!("the universe of sweep '{}' could not be listed", sweep.label)));
        };
        let Some(ignored) = binary_list(b, inputs.project_root, &["--ignored"], &env_refs, &runtime)? else {
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
        prepare_lane(&self.inputs, sweep, &[], &[])
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
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub(crate) fn prepare_profile(
    sweeps: &[ResolvedSweep],
    doctests: bool,
    preparer: &mut dyn Preparer,
) -> ProfilePrep {
    let mut plan = AccountingPlan { run_id: new_run_id(), ..AccountingPlan::default() };
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
            lane: idx,
            label: sweep.label.clone(),
            kind,
            shape: shape.clone(),
            prepared: false,
            include_ignored: sweep.libtest_args.iter().any(|a| a == "--include-ignored"),
            doc_carrier: lane_runs_doctests(sweep, doctests),
            // A doc-only lane always; a serial carrier once its binaries are
            // known to hold a library (below).
            doc_streams_required: kind == LaneKind::DocOnly,
            executions: Vec::new(),
            ignored_selected: Vec::new(),
            outside_claim: Vec::new(),
            artifacts: Vec::new(),
        };
        if stopped || crate::shutdown::is_shutdown_requested() {
            if !stopped {
                plan.incomplete.push("preparation stopped before every lane was prepared".into());
            }
            stopped = true;
            for f in &sweep.declared_filters {
                ledger.record_unknown(f, &sweep.label);
            }
            plan.lanes.push(record);
            lanes.push(None);
            continue;
        }
        // The support executables are hashed AFTER the lane's own build, the
        // order the lane's verification repeats: that build can re-uplift a
        // support bin over the pre-build's, and the file the tests read is
        // whichever landed last.
        let prepared = match preparer.support(sweep).and_then(|support| {
            let mut p = preparer.lane(sweep)?;
            p.support_fingerprint = support_fingerprint(&support)?;
            Ok(p)
        }) {
            Ok(p) => p,
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
                for f in &sweep.declared_filters {
                    ledger.record_unknown(f, &sweep.label);
                }
                if error.is_none() {
                    error = Some(e);
                }
                plan.lanes.push(record);
                lanes.push(None);
                continue;
            }
        };

        // Each resolution of a non-doc lane needs its shape's universe, once.
        let mut lane_ok = true;
        if kind != LaneKind::DocOnly {
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
        if kind != LaneKind::DocOnly && lane_ok {
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
        } else if kind != LaneKind::DocOnly {
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
        let prep = prepare_profile(&[sweep("one")], false, &mut fake);
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
        let prep = prepare_profile(&[sweep("one"), sweep("two"), sweep("three")], false, &mut fake);
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
        let prep = prepare_profile(&[sweep("one")], false, &mut Elsewhere);
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
        let prep = prepare_profile(std::slice::from_ref(&with_support), false, &mut fake);
        assert!(prep.plan.complete, "{:?}", prep.plan.incomplete);
        assert_eq!(fake.lanes_prepared, vec!["support:one", "one"]);

        let mut failing = Fake { support_fails: true, ..Fake::new(None) };
        let prep = prepare_profile(&[with_support], false, &mut failing);
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
        let prep = prepare_profile(std::slice::from_ref(&with_support), false, &mut fake);
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
            complete: true,
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
}
