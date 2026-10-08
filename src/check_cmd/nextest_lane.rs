// The `harness = "nextest"` lane: the linked nextest engine as a fast
// process-per-test EXECUTOR for a brokkr-configured sweep.
//
// An engine, not a policy source. Everything the lane runs and claims comes
// from brokkr.toml - that is the entire point of brokkr standing over a
// checkout it does not own - so the engine executes under a config brokkr
// SYNTHESIZES per run: retries zero, no default-filter, no test groups, no
// setup scripts, and `slow-timeout` + `terminate-after` set to brokkr's own
// per-test watchdog ceiling, which gives every test the hang kill the other
// lanes have, per process, for free. The project's `.config/nextest.toml` is
// NEVER opened and `NEXTEST_PROFILE` is never read: a foreign config must
// not get a vote in what a brokkr gate runs, and an earlier version of this
// lane that honoured it had to grow a pin/bless/drift apparatus just to
// police the vote it had granted.
//
// What the engine is FOR: dissolving per-test serialization. A sweep that
// needs process isolation (the `isolation = "process"` semantics) is N
// sequential `cargo test -- --exact` spawns on the libtest path; here it is
// the same guarantee executed concurrently. The in-process libtest lanes
// remain the default everywhere else - a shared-process lane is the only
// detector for the process-global-state class, and this lane deliberately
// does not replace it.
//
// The division of labour:
// - Brokkr owns the COMPILE SHAPE. The build is brokkr's cargo invocation
//   (selection, features, unification pin, cargo profile, [lints] allows,
//   sweep env, rustflags plumbing), streamed into nextest's
//   BinaryListBuilder - so a nextest lane and a libtest lane with equal
//   compile inputs share the target dir and the clippy dedupe.
// - Brokkr owns the FILTERS. The sweep's `only`/`skip` map onto nextest's
//   own TestFilterPatterns (identical libtest substring semantics, no
//   filterset string to escape); a package-qualified skip becomes the
//   filterset `not (package(P) & test(~X))`, folding the one predicate
//   brokkr used to evaluate itself back into the tool.
// - Brokkr owns CONCURRENCY: the profile's `test_threads` maps onto the
//   engine's in-flight count (unset = the engine's num-cpus default).
// - The engine owns execution and rendering: process-per-test scheduling
//   and its reporter. Brokkr wraps it in the usual sweep lines, records the
//   engine's own events (starts, finishes, cancellation) before the reporter
//   sees them, and decides pass/fail from the run's outcome.
//
// One listing, kept: the `TestList` the preparation builds is the one the
// lane executes. Building a second at run time would let the plan and the run
// disagree about what the lane selects, which is the one thing a plan is for.
//
// The linked engine's version is brokkr's pin, not the host's. It is
// recorded below and printed on the lane's header line, so a result says
// which engine produced it.

use camino::Utf8PathBuf;
use guppy::graph::PackageGraph;
use nextest_filtering::{Filterset, FiltersetKind, ParseContext};
use nextest_runner::{
    cargo_config::{CargoConfigs, EnvironmentMap},
    config::core::{EvaluatableProfile, NextestConfig},
    double_spawn::DoubleSpawnInfo,
    input::InputHandlerKind,
    list::{
        BinaryListBuilder, ListProgressOptions, RustTestArtifact, TestExecuteContext, TestList,
    },
    platform::{BuildPlatforms, HostPlatform, PlatformLibdir},
    helpers::{ShowTerminalProgress, ThemeCharacters},
    reporter::{
        ReporterBuilder, ReporterOutput, ShowProgress,
        events::{
            CancelReason, ExecutionResultDescription, FailureDescription, ReporterEvent,
            RunFinishedStats, RunOutcome, RunStats, TestEventKind,
        },
        structured::StructuredReporter,
    },
    reuse_build::PathMapper,
    run_mode::NextestRunMode,
    runner::{TestRunnerBuilder, VersionEnvVars, configure_handle_inheritance},
    signal::SignalHandlerKind,
    target_runner::TargetRunner,
    test_filter::{FilterBound, RunIgnored, TestFilter, TestFilterPatterns},
};

/// The linked nextest-runner version, printed on the lane header so a result
/// names the engine that produced it. Must track Cargo.toml's pin - there is
/// no runtime accessor on the crate, and a compile-time drift here would
/// only mislabel output, never change behaviour.
const NEXTEST_ENGINE_VERSION: &str = "0.126.0";

/// The engine config brokkr synthesizes for one run. Written under the
/// brokkr-owned state dir (never into the code tree) and handed to the
/// engine as its ONLY config file, which is what replaces - not merely
/// shadows - any `.config/nextest.toml` the checkout carries.
fn write_synthesized_config(state_root: &Path, ceiling: std::time::Duration) -> Result<Utf8PathBuf, DevError> {
    let dir = state_root.join(".brokkr");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("nextest-synth.toml");
    let ceiling = ceiling.as_secs().max(1);
    // `terminate-after = 1`: the first time a test crosses the ceiling it is
    // terminated, the exact semantics of the libtest lanes' watchdog.
    //
    // `grace-period = "0s"`: the cap is terminal AT the ceiling. A grace period
    // sends SIGTERM at the ceiling and only SIGKILLs afterwards, so a test that
    // ignores or blocks SIGTERM outlives its budget by exactly that much - and the
    // budget is a hard cap, not a request.
    let body = format!(
        "# Generated by brokkr on every nextest-lane run; do not edit.\n\
         [profile.default]\n\
         retries = 0\n\
         slow-timeout = {{ period = \"{ceiling}s\", terminate-after = 1, grace-period = \"0s\" }}\n"
    );
    std::fs::write(&path, body)?;
    Utf8PathBuf::from_path_buf(path)
        .map_err(|p| DevError::Config(format!("state dir is not UTF-8: {}", p.display())))
}

/// The cargo config set the engine reads, carrying every env var its test
/// processes (and listing runs) must see.
///
/// The engine builds its own child commands, so neither the spawn choke point
/// (`crate::hold::stamp`) nor a `Command::env` can reach them; what it does
/// apply to every process it starts is cargo's `[env]` table. Two generated
/// config files ride in that way:
///
/// - the compilation capability, rustc-info cache switch and orphan-reap
///   token (`crate::hold::cargo_config_overrides`);
/// - the sweep env - `[[check]] env`, a profile's `env`, `BROKKR_TEST_BIN_DIR`,
///   nidhogg's `CARGO_TARGET_TMPDIR`: the same pairs the libtest lanes set on
///   `cargo test`. Before this file existed those pairs reached only
///   `cargo metadata` and the `--no-run` build, and a profile's
///   `env = { BROKKR_TEST_PLATFORM = "1" }` was a silent no-op on this lane.
///
/// Every sweep entry is `force = true`, matching the libtest lanes, where
/// `Command::env` overrides whatever brokkr inherited; `relative = false` for
/// the reason `hold::cargo_config_overrides` gives. The two hold variables and
/// the orphan-reap token are left out of the sweep file so the capability file
/// alone decides them - the libtest lanes stamp them last, after the sweep env,
/// for the same reason.
fn engine_cargo_configs(state_root: &Path, env: &[(&str, &str)]) -> Result<CargoConfigs, DevError> {
    let mut overrides = crate::hold::cargo_config_overrides()
        .map_err(|e| DevError::Config(format!("nextest env config unwritable: {e}")))?;
    overrides.push(write_sweep_env_config(state_root, env)?);
    CargoConfigs::new(overrides)
        .map_err(|e| DevError::Config(format!("cargo config discovery failed: {e}")))
}

/// The sweep env as a cargo `[env]` config body. Split from the write so the
/// rendering is testable without touching disk.
fn sweep_env_config_body(env: &[(&str, &str)]) -> String {
    let mut table = toml_edit::Table::new();
    for (key, value) in env {
        if *key == crate::hold::CAPABILITY_ENV
            || *key == crate::hold::RUSTC_INFO_CACHE_ENV
            || *key == crate::test_orphans::MARKER_ENV
        {
            continue;
        }
        let mut entry = toml_edit::InlineTable::new();
        entry.insert("value", (*value).into());
        entry.insert("force", true.into());
        entry.insert("relative", false.into());
        table[*key] = toml_edit::value(entry);
    }
    let mut doc = toml_edit::DocumentMut::new();
    doc["env"] = toml_edit::Item::Table(table);
    format!("# Generated by brokkr on every nextest-lane run; do not edit.\n{doc}")
}

/// Write [`sweep_env_config_body`] under the brokkr-owned state dir (never the
/// code tree) and return its path for `CargoConfigs::new`, which reads it
/// eagerly. Rewritten from scratch on every call so a previous sweep's env
/// cannot leak into this one; owner-only, since sweep env can carry anything.
fn write_sweep_env_config(state_root: &Path, env: &[(&str, &str)]) -> Result<String, DevError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let dir = state_root.join(".brokkr");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("nextest-sweep-env.toml");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(sweep_env_config_body(env).as_bytes())?;
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| DevError::Config(format!("state dir is not UTF-8: {}", path.display())))
}

/// The engine lane, prepared: the listing it will execute, and everything the
/// runner is built from.
///
/// The `TestList` borrows the package graph, and the profile borrows the
/// config, for as long as either lives - and here that is from the `prepare`
/// phase to the end of the lane, across every other lane's preparation. Both
/// are leaked to get that lifetime: a `brokkr check` run is one process, there
/// is one graph and one config per engine lane, and the alternative - building
/// the list again at run time - is exactly the second listing a plan must not
/// have.
pub(crate) struct NextestPrepared {
    test_list: TestList<'static>,
    profile: EvaluatableProfile<'static>,
    cargo_configs: CargoConfigs,
    target_runner: TargetRunner,
    double_spawn: DoubleSpawnInfo,
    version_env_vars: VersionEnvVars,
    /// nextest binary id -> unit, through the artifact index: how the engine's
    /// events are attributed.
    by_id: HashMap<String, BinaryUnit>,
}

/// The engine lane's build, read as brokkr's artifact index.
struct NextestArtifacts {
    binaries: Vec<TestBinary>,
    /// nextest binary id -> unit.
    by_id: HashMap<String, BinaryUnit>,
    /// The runtime index's fingerprint ([`BuildRuntimeIndex::fingerprint`]):
    /// the build-script facts and support bins the engine's launch env is
    /// derived from, which the lane's re-verification (and a replay's)
    /// compares against a fresh build. Without it a nextest lane's
    /// verification checked its test executables only, and a support bin
    /// rebuilt by another lane since the plan ran unnoticed.
    runtime_fingerprint: Option<Vec<String>>,
}

/// Read the engine lane's artifact stream: the units the engine's ids map
/// onto, the executables the plan hashes, and the runtime fingerprint
/// verification needs.
fn nextest_lane_artifacts(stdout: &str) -> Result<NextestArtifacts, DevError> {
    let (binaries, index, _) = parse_test_binaries(stdout);
    let by_id = binaries
        .iter()
        .map(|b| {
            let u = BinaryUnit::of(b);
            (u.id(), u)
        })
        .collect();
    let runtime_fingerprint = Some(index.fingerprint()?);
    Ok(NextestArtifacts { binaries, by_id, runtime_fingerprint })
}

/// What the engine's listing selects.
pub(crate) enum EngineFilter<'a> {
    /// A sweep's own `only`/`skip`/`include-ignored`.
    Sweep(&'a ResolvedSweep),
    /// Exactly these (nextest binary id, test name) executions - a replay's
    /// selection. Both halves, because two binaries of one package may define
    /// the same test path, and a name alone would run the one that passed.
    Exact { pairs: &'a [(String, String)], ignored_mode: IgnoredMode },
}

/// What the engine resolves from cargo configuration when it launches, as
/// lines, read from the DISCOVERED config files and environment alone (brokkr's
/// own generated configs are rebuilt from recorded inputs and are left out):
/// the `[env]` tables its test processes get and the target runner they are
/// started through. The engine builds its own children, so this is the part of
/// the launch environment brokkr cannot record by value; it records it as a
/// fingerprint and a replay refuses on any difference ([`engine_launch_drift`]).
/// Debug renderings: the engine's types offer no iteration, and the renderings
/// are deterministic for the pinned engine version.
fn engine_launch_fingerprint(target_runner: &TargetRunner) -> Result<Vec<String>, DevError> {
    let discovered = CargoConfigs::new(std::iter::empty::<&str>())
        .map_err(|e| DevError::Config(format!("cargo config discovery failed: {e}")))?;
    let env = EnvironmentMap::new(&discovered);
    Ok(vec![format!("cargo-env {env:?}"), format!("runner {target_runner:?}")])
}

/// What differs between the engine launch a run recorded and the one resolved
/// now. Empty when they agree.
pub(crate) fn engine_launch_drift(recorded: &[String], now: &[String]) -> Vec<String> {
    let mut out: Vec<String> = recorded
        .iter()
        .filter(|l| !now.contains(l))
        .map(|l| format!("the engine's launch environment `{l}` no longer holds"))
        .collect();
    out.extend(
        now.iter()
            .filter(|l| !recorded.contains(l))
            .map(|l| format!("the engine's launch environment gained `{l}`")),
    );
    out
}

/// Prepare one `harness = "nextest"` sweep: build its shape, read the artifact
/// stream into the engine, list under the sweep's filters, and keep the list.
fn prepare_nextest(
    inputs: &LaneInputs<'_>,
    sweep: &ResolvedSweep,
    packages: &[&str],
    extra_args: &[String],
    env: &LaneEnv,
) -> Result<PreparedLane, DevError> {
    let (cargo_extra, libtest_extra) = split_extra_args(extra_args);
    if !libtest_extra.is_empty() {
        return Err(DevError::Config(format!(
            "sweep '{}' runs under the nextest engine, which takes no raw libtest args; drop \
             the trailing `-- {}` (use the sweep's `skip`/`only`).",
            sweep.label,
            libtest_extra.join(" ")
        )));
    }
    reject_unsupported_forwarded(sweep, cargo_extra)?;
    let mut selection = env.allow_args.clone();
    selection.extend(sweep_selection_args(sweep, packages));
    selection.extend(cargo_extra.iter().cloned());
    let ignored_mode = effective_ignored_mode(sweep.libtest_args.iter().map(String::as_str));
    build_nextest_lane(inputs, &sweep.label, selection, env, &EngineFilter::Sweep(sweep), ignored_mode)
}

/// The host as the engine's build platform (no cross target).
fn host_build_platforms() -> Result<BuildPlatforms, DevError> {
    let host = HostPlatform::detect(PlatformLibdir::from_rustc_stdout(
        nextest_runner::RustcCli::print_host_libdir().read(),
    ))
    .map_err(|e| DevError::Build(format!("nextest host platform detection failed: {e}")))?;
    Ok(BuildPlatforms { host, target: None })
}

/// The engine lane's preparation, from a cargo `selection` and a filter: the
/// one place the engine is built and its listing taken, for a sweep's own
/// preparation and for a replay of recorded executions alike.
pub(crate) fn build_nextest_lane(
    inputs: &LaneInputs<'_>,
    label: &str,
    selection: Vec<String>,
    env: &LaneEnv,
    filter: &EngineFilter<'_>,
    ignored_mode: IgnoredMode,
) -> Result<PreparedLane, DevError> {
    let env_refs = env.refs();
    let project_root = inputs.project_root;

    // The graph feeds config parsing (filterset predicates, test groups) and
    // artifact resolution. `--all-features --filter-platform` mirrors what
    // cargo-nextest itself asks for - the graph is a naming universe, not the
    // build's resolution.
    let build_platforms = host_build_platforms()?;
    let triple = build_platforms.host.platform.triple_str().to_owned();

    let metadata = output::run_captured_with_env(
        "cargo",
        &["metadata", "--format-version=1", "--all-features", "--filter-platform", &triple],
        project_root,
        &env_refs,
    )?;
    if !metadata.status.success() {
        output::error(&String::from_utf8_lossy(&metadata.stderr));
        return Err(DevError::Build("cargo metadata failed".into()));
    }
    let metadata_json = String::from_utf8_lossy(&metadata.stdout).into_owned();

    // The build is brokkr's: the same compile-shape argv every other lane
    // uses, streamed into nextest's builder so the artifact facts (binary
    // ids, build meta, dylib paths) are nextest's own reading of it.
    let mut args: Vec<String> = vec!["test".into(), "--no-run".into(), "--message-format=json".into()];
    args.extend(selection.iter().cloned());
    if !has_target_selector(&args) {
        args.push("--tests".into());
    }
    cargo_line(inputs.commands, &format!("cargo {}", args.join(" ")));
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let build = output::run_captured_with_env("cargo", &arg_refs, project_root, &env_refs)?;
    if !build.status.success() {
        output::error(&format!("failing command: cargo {}", args.join(" ")));
        output::error(&String::from_utf8_lossy(&build.stderr));
        return Err(DevError::Reported(format!("sweep '{label}' could not be prepared")));
    }
    // Cargo exited 0; did it actually say anything? Without a `build-finished`
    // record this stdout is not cargo's artifact stream, and an empty case set
    // read from it would be accepted as an empty selection.
    require_build_finished(&build.stdout, &args)?;
    let stdout = String::from_utf8_lossy(&build.stdout).into_owned();
    assemble_nextest_lane(
        inputs,
        label,
        selection,
        env,
        filter,
        ignored_mode,
        &NextestBuild { metadata_json: &metadata_json, stdout: &stdout, build_platforms, ceiling: test_runner::TEST_TIMEOUT },
    )
}

/// What a cargo build handed the engine: the `cargo metadata` document, the
/// `--message-format=json` artifact stream, the platforms, and the per-test cap
/// the synthesized config carries.
pub(crate) struct NextestBuild<'a> {
    pub(crate) metadata_json: &'a str,
    pub(crate) stdout: &'a str,
    pub(crate) build_platforms: BuildPlatforms,
    pub(crate) ceiling: std::time::Duration,
}

/// The engine lane's preparation from a build's products: everything
/// [`build_nextest_lane`] does after cargo has run. Split out so the part that
/// reads the build into the engine, lists, and captures the launch fingerprint
/// can be driven without cargo.
#[allow(clippy::too_many_lines)]
pub(crate) fn assemble_nextest_lane(
    inputs: &LaneInputs<'_>,
    label: &str,
    selection: Vec<String>,
    env: &LaneEnv,
    filter: &EngineFilter<'_>,
    ignored_mode: IgnoredMode,
    build: &NextestBuild<'_>,
) -> Result<PreparedLane, DevError> {
    let env_refs = env.refs();
    let NextestBuild { metadata_json, stdout, build_platforms, ceiling } = build;
    let (stdout, ceiling) = (*stdout, *ceiling);
    let build_platforms = build_platforms.clone();
    let graph: &'static PackageGraph = Box::leak(Box::new(
        PackageGraph::from_json(metadata_json)
            .map_err(|e| DevError::Build(format!("cargo metadata unparseable: {e}")))?,
    ));
    let workspace_root: Utf8PathBuf = graph.workspace().root().to_owned();
    let cargo_configs = engine_cargo_configs(inputs.state_root, &env_refs)?;
    let mut builder = BinaryListBuilder::new(graph, build_platforms.clone());
    for line in stdout.lines() {
        builder
            .process_message_line(line)
            .map_err(|e| DevError::Build(format!("nextest could not read the build: {e}")))?;
    }
    let binary_list = std::sync::Arc::new(builder.finish());
    // The same stream, read as brokkr's artifact index: the units the engine's
    // ids are mapped onto, and the executables the plan hashes.
    let artifacts = nextest_lane_artifacts(stdout)?;
    let lane_binaries = artifacts.binaries;
    let by_id = artifacts.by_id;

    // The engine's config is brokkr's own synthesized file, passed as the
    // explicit config source so the checkout's `.config/nextest.toml` (if
    // any) is never consulted, and `NEXTEST_PROFILE` is never read: a
    // foreign config gets no vote in what a brokkr sweep runs.
    let synth = write_synthesized_config(inputs.state_root, ceiling)?;
    let pcx = ParseContext::new(graph);
    let config: &'static NextestConfig = Box::leak(Box::new(
        NextestConfig::from_sources(
            workspace_root.clone(),
            &pcx,
            Some(synth.as_path()),
            std::iter::empty::<&nextest_runner::config::core::ToolConfigFile>(),
            &std::collections::BTreeSet::new(),
        )
        .map_err(|e| DevError::Config(format!("nextest engine config: {e}")))?,
    ));
    let early_profile = config
        .profile(NextestConfig::DEFAULT_PROFILE)
        .map_err(|e| DevError::Config(format!("nextest engine profile: {e}")))?;
    let known_groups = early_profile.known_groups();
    let profile = early_profile.apply_build_platforms(&build_platforms);

    // Filters: `only` and unqualified `skip` ride nextest's own libtest
    // pattern emulation (same substring semantics, nothing to escape);
    // package-qualified skips become one filterset each.
    let test_filter = match filter {
        EngineFilter::Sweep(sweep) => sweep_engine_filter(sweep, &pcx, &known_groups)?,
        EngineFilter::Exact { pairs, ignored_mode } => exact_engine_filter(pairs, *ignored_mode, &pcx, &known_groups)?,
    };

    // Double-spawn re-invokes the CURRENT executable with a `__double-spawn`
    // subcommand - a protocol cargo-nextest's own binary implements and
    // brokkr does not, so enabling it makes every test command a brokkr clap
    // error. Disabled is a supported nextest mode (NEXTEST_DOUBLE_SPAWN=0);
    // the cost is a narrow unix signal race around spawn, not correctness of
    // results.
    let double_spawn = DoubleSpawnInfo::disabled();
    let target_runner = TargetRunner::new(&cargo_configs, &build_platforms)
        .map_err(|e| DevError::Config(format!("nextest target runner resolution failed: {e}")))?;
    let engine_launch = engine_launch_fingerprint(&target_runner)?;
    let version_env_vars = VersionEnvVars {
        current_version: NEXTEST_ENGINE_VERSION
            .parse()
            .map_err(|e| DevError::Build(format!("engine version constant: {e}")))?,
        required_version: None,
        recommended_version: None,
    };
    let ctx = TestExecuteContext {
        run_id: nextest_runner::helpers::force_or_new_run_id(),
        version_env_vars: &version_env_vars,
        profile_name: profile.name(),
        double_spawn: &double_spawn,
        target_runner: &target_runner,
    };
    let path_mapper = PathMapper::noop();
    let rust_build_meta = binary_list.rust_build_meta.map_paths(&path_mapper);
    let test_artifacts = RustTestArtifact::from_binary_list(
        graph,
        std::sync::Arc::clone(&binary_list),
        &rust_build_meta,
        &path_mapper,
        None,
    )
    .map_err(|e| DevError::Build(format!("nextest artifact resolution: {e}")))?;
    let env_map = EnvironmentMap::new(&cargo_configs);
    let test_list = TestList::new(
        &ctx,
        test_artifacts,
        rust_build_meta,
        &test_filter,
        None,
        workspace_root,
        env_map,
        &profile,
        // `All`, explicitly: the synthesized config has no default-filter,
        // and the concept must never re-enter through a bound.
        FilterBound::All,
        nextest_runner::config::core::get_num_cpus(),
        ListProgressOptions::new(
            ShowProgress::None,
            ShowTerminalProgress::from_cargo_configs(&cargo_configs, false),
            ThemeCharacters::default(),
            false,
        ),
    )
    .map_err(|e| DevError::Build(format!("nextest listing failed: {e}")))?;

    if test_list.run_count() == 0 {
        return Err(DevError::Config(format!(
            "cargo test: zero tests ran (sweep: {label}) - the sweep's filters selected no work; \
             treat as a wrong-run."
        )));
    }

    // The engine's per-testcase verdicts, onto brokkr's units. `Selected` is
    // the only executing verdict; `Ignored` already encodes the
    // include-ignored policy; a verdict the ledger has no policy for refuses
    // the lane - an unaudited reason cannot be accounted.
    let mut selected: BTreeMap<BinaryUnit, (Vec<String>, BTreeSet<String>)> = BTreeMap::new();
    for t in test_list.iter_tests() {
        let id = t.id().binary_id.to_string();
        let test = t.id().test_name.to_string();
        let Some(unit) = by_id.get(&id) else {
            return Err(DevError::Build(format!(
                "the engine listed binary {id}, which the build's artifact stream does not hold"
            )));
        };
        let slot = selected.entry(unit.clone()).or_default();
        match nextest_disposition(&t.test_info.filter_match) {
            Disposition::Selected => slot.0.push(test),
            Disposition::Ignored => {
                slot.0.push(test.clone());
                slot.1.insert(test);
            }
            Disposition::Unmatched => {}
            Disposition::Unclassified => {
                return Err(DevError::Build(format!(
                    "the engine reported a filter verdict brokkr has no policy for on {id}::{test} - \
                     an unaudited reason cannot be accounted, so the lane refuses."
                )));
            }
        }
    }
    let mut binaries = Vec::new();
    for b in &lane_binaries {
        let unit = BinaryUnit::of(b);
        let (names, ignored) = selected.remove(&unit).unwrap_or_default();
        let hash = hash_file(Path::new(&b.executable))
            .map_err(|e| DevError::Build(format!("could not hash {}: {e}", b.executable)))?;
        // No benchmarks: the engine never runs `#[bench]` functions in test mode.
        binaries.push(PreparedBinary {
            binary: b.clone(),
            unit,
            hash,
            envelope: None,
            unlisted: None,
            selected: names,
            ignored,
            benchmarks: Vec::new(),
        });
    }

    let mut prepared = PreparedLane::bare(env.clone());
    prepared.runtime_fingerprint = artifacts.runtime_fingerprint;
    prepared.set_ignored_mode(ignored_mode);
    prepared.engine_launch = engine_launch;
    prepared.enumerated = lane_binaries.len();
    prepared.resolutions.push(PreparedResolution { resolution: None, selection, binaries });
    prepared.nextest = Some(NextestPrepared {
        test_list,
        profile,
        cargo_configs,
        target_runner,
        double_spawn,
        version_env_vars,
        by_id,
    });
    Ok(prepared)
}

/// A finished engine test as a terminal record. A death the lane's own
/// cancellation caused - the fail-fast stop signals every test still running -
/// is an interruption, not a failure of that test; an exit-code failure after
/// the cancellation began is still the test's own. Any timeout is a timeout,
/// whatever the engine config would have made of it.
fn engine_result(result: &ExecutionResultDescription, cancelling: bool) -> TestResult {
    use nextest_runner::config::elements::LeakTimeoutResult;
    match result {
        ExecutionResultDescription::Pass
        | ExecutionResultDescription::Leak { result: LeakTimeoutResult::Pass } => TestResult::Ok,
        ExecutionResultDescription::Timeout { .. } => TestResult::TimedOut,
        ExecutionResultDescription::Fail { failure: FailureDescription::Abort { .. }, .. } if cancelling => {
            TestResult::Interrupted
        }
        _ => TestResult::Failed,
    }
}

/// What an engine cancellation is, as a termination cause.
fn cancel_cause(reason: Option<CancelReason>) -> TerminationCause {
    match reason {
        Some(CancelReason::ReportError) => TerminationCause::EngineError,
        Some(CancelReason::GlobalTimeout) => TerminationCause::RunDeadline,
        Some(CancelReason::Signal | CancelReason::Interrupt | CancelReason::SecondSignal) => stop_cause(),
        _ => TerminationCause::FailFast,
    }
}

/// Run one prepared `harness = "nextest"` sweep. Returns `Ok(false)` when the
/// run failed, having already reported it. An `Err` leaving this lane carries
/// its whole diagnostic in the message; `run_test_phase`'s caller voices it.
fn run_nextest_sweep(
    sweep: &ResolvedSweep,
    packages: &[&str],
    prepared: &PreparedLane,
    tap: &LaneTap,
    commands: bool,
) -> Result<bool, DevError> {
    let Some(np) = &prepared.nextest else {
        return Err(DevError::Build(format!("sweep '{}' reached the engine lane unprepared", sweep.label)));
    };
    announce_sweep(
        &format!(
            "test {}: {}, nextest engine {NEXTEST_ENGINE_VERSION}, process-per-test, brokkr-owned config",
            sweep.label,
            describe_sweep(sweep, true, packages)
        ),
        None,
        commands,
    );
    run_nextest_engine(&sweep.label, sweep.test_threads, np, tap).map(|r| r.passed)
}

/// How an engine run ended, structurally: the verdict and whether any test
/// blew its per-test budget. A timeout is its own disposition - a replay stops
/// on it - and not something to be recovered from the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EngineRun {
    pub(crate) passed: bool,
    pub(crate) timed_out: bool,
}

/// Execute a prepared engine lane under the lane's own policy. The part of
/// [`run_nextest_sweep`] a replay shares: it needs no sweep, only the label
/// the lines carry and the recorded in-flight count.
#[allow(clippy::too_many_lines)]
pub(crate) fn run_nextest_engine(
    label: &str,
    test_threads: Option<u32>,
    np: &NextestPrepared,
    tap: &LaneTap,
) -> Result<EngineRun, DevError> {
    // Concurrency is brokkr policy: the profile's `test_threads` maps onto
    // the engine's in-flight count. Unset (or 0) leaves the engine's
    // num-cpus default; there is no serial-by-default here, because
    // process-per-test needs no watchdog attribution trick - the synthesized
    // slow-timeout kills a hung test by name at any concurrency.
    let mut runner_builder = TestRunnerBuilder::default();
    // Fail-fast, and this is a trade rather than a preference.
    //
    // Every other brokkr lane enumerates all failures in one run (the libtest
    // lanes' `--no-fail-fast`), and `MaxFail::All` did that here. But in this
    // engine a timeout is delivered as a failed test, and `MaxFail::All` means
    // "run everything regardless of failures" - so a test that blew its 20s cap
    // was terminated while the run carried on through every remaining test,
    // returning an ordinary red sweep at the end. The contract says a blown
    // budget stops brokkr, and the engine offers no way to cancel on a timeout
    // without cancelling on a failure.
    //
    // So this lane stops at the first failure of any kind. The cost is real: a
    // nextest sweep no longer lists every failing test in one run. The cap is not
    // negotiable and the failure list is, so the cap wins.
    // `Immediate`, not `Wait`: once the lane is stopping, tests still running are
    // signalled rather than allowed to finish. Waiting would let a sibling keep
    // burning wall time after brokkr has already decided the run is over.
    runner_builder.set_max_fail(nextest_runner::config::elements::MaxFail::Count {
        max_fail: 1,
        terminate: nextest_runner::config::elements::TerminateMode::Immediate,
    });
    if let Some(n) = test_threads
        && n >= 1
    {
        let count = i64::from(n).try_into().map_err(|_| {
            DevError::Config(format!("test_threads = {n} does not fit the engine's count"))
        })?;
        runner_builder
            .set_test_threads(nextest_runner::config::elements::TestThreads::Count(count));
    }
    let run_id = nextest_runner::helpers::force_or_new_run_id();
    let runner = runner_builder
        .build(
            run_id,
            np.version_env_vars.clone(),
            &np.test_list,
            &np.profile,
            std::env::args().collect(),
            // The engine's signal handling is process-global and permanent;
            // a unit test that drives a real engine run must not leave it in
            // the test binary.
            if cfg!(test) { SignalHandlerKind::Noop } else { SignalHandlerKind::Standard },
            InputHandlerKind::Noop,
            np.double_spawn.clone(),
            np.target_runner.clone(),
        )
        .map_err(|e| DevError::Build(format!("nextest runner: {e}")))?;

    let mut reporter = ReporterBuilder::default().build(
        &np.test_list,
        &np.profile,
        ShowTerminalProgress::from_cargo_configs(&np.cargo_configs, false),
        ReporterOutput::Terminal,
        StructuredReporter::new(),
    );

    configure_handle_inheritance(false)
        .map_err(|e| DevError::Build(format!("nextest handle setup: {e}")))?;
    // One engine stream per lane: every test is its own process, and every
    // record names its binary, so attribution needs no per-process stream.
    let stream = crate::test_runner::next_stream_id();
    let observe = |unit_id: String, event: ObsEvent| {
        let origin = match np.by_id.get(&unit_id) {
            Some(unit) => StreamOrigin::Engine { resolution: None, unit: unit.clone() },
            None => StreamOrigin::Unattributed { detail: format!("engine binary {unit_id}") },
        };
        tap.record(JournalRecord::Observed { lane: tap.lane(), stream, origin, event });
    };
    // The run's outcome carries no counts; they ride the engine's own
    // `RunFinished` event, which every completed run emits once.
    let mut finished: Option<RunStats> = None;
    let mut cancelling = false;
    let mut timed_out = false;
    let outcome = runner.try_execute(|event| {
        // Recorded BEFORE the reporter sees it: a reporter that fails must not
        // take the evidence with it.
        if let ReporterEvent::Test(e) = &event {
            match &e.kind {
                TestEventKind::TestStarted { test_instance, .. }
                | TestEventKind::TestRetryStarted { test_instance, .. } => observe(
                    test_instance.binary_id.to_string(),
                    ObsEvent::Started { name: test_instance.test_name.to_string() },
                ),
                TestEventKind::TestFinished { test_instance, run_statuses, .. } => {
                    let result = engine_result(&run_statuses.last_status().result, cancelling);
                    timed_out |= result == TestResult::TimedOut;
                    observe(
                        test_instance.binary_id.to_string(),
                        ObsEvent::Finished { name: test_instance.test_name.to_string(), result },
                    );
                }
                TestEventKind::RunBeginCancel { current_stats, .. }
                | TestEventKind::RunBeginKill { current_stats, .. } => {
                    if !cancelling {
                        cancelling = true;
                        tap.terminate(
                            TerminationScope::Lane,
                            cancel_cause(current_stats.cancel_reason),
                            None,
                        );
                    }
                }
                TestEventKind::RunFinished { run_stats: RunFinishedStats::Single(s), .. } => {
                    finished = Some(*s);
                }
                _ => {}
            }
        }
        reporter.report_event(event)
    });
    // `Reporter::finish` is infallible and returns only `ReporterStats`, which
    // this lane does not use.
    let _ = reporter.finish();
    tap.record(JournalRecord::Observed {
        lane: tap.lane(),
        stream,
        origin: StreamOrigin::Unattributed { detail: "the engine lane's stream".into() },
        event: ObsEvent::StreamEnded { end: crate::test_runner::StreamEnd::Eof },
    });
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            // The events up to the failure are already recorded; the failure
            // is recorded as what it is, then voiced.
            if !cancelling {
                tap.terminate(TerminationScope::Lane, TerminationCause::EngineError, None);
            }
            return Err(DevError::Build(format!("nextest run failed to execute: {e}")));
        }
    };

    let passed = matches!(outcome, RunOutcome::Success);
    if passed {
        // A success with no `RunFinished` would be a run brokkr cannot count,
        // so it is not passed off as one with zero tests.
        let Some(run_stats) = finished else {
            return Err(DevError::Build(
                "nextest reported success without a run-finished event".into(),
            ));
        };
        // Into the grouped test line like every other lane. The engine's
        // `skipped` folds filtered-out and ignored tests into one number, so
        // it goes to the log rather than being passed off as either.
        note_tests(run_stats.passed, 0, 0);
        output::detail(&format!(
            "test {label}: {} passed, {} skipped by the engine",
            run_stats.passed, run_stats.skipped
        ));
    }
    Ok(EngineRun { passed, timed_out })
}

/// The engine filter for a replay: exactly the recorded (binary id, test)
/// executions, as one filterset ([`engine_exact_filterset`]), under the lane's
/// recorded `#[ignore]` policy. No substring patterns and none of the sweep's
/// own `skip`s - the selection is already resolved to full names.
fn exact_engine_filter(
    pairs: &[(String, String)],
    ignored_mode: IgnoredMode,
    pcx: &ParseContext<'_>,
    known_groups: &nextest_filtering::KnownGroups,
) -> Result<TestFilter, DevError> {
    let expr = engine_exact_filterset(pairs).map_err(DevError::Config)?;
    let parsed = Filterset::parse(expr.clone(), pcx, FiltersetKind::Test, known_groups)
        .map_err(|e| DevError::Config(format!("replay selection `{expr}` did not compile: {e:?}")))?;
    let run_ignored = match ignored_mode {
        IgnoredMode::Exclude => RunIgnored::default(),
        IgnoredMode::Include => RunIgnored::All,
        IgnoredMode::Only => RunIgnored::Only,
    };
    TestFilter::new(NextestRunMode::Test, run_ignored, TestFilterPatterns::default(), vec![parsed])
        .map_err(|e| DevError::Config(format!("nextest test filter: {e}")))
}

/// A successful cargo status alone cannot establish which binaries were built.
/// Both execution and coverage enumeration need the completed artifact stream.
fn require_build_finished(stdout: &[u8], args: &[String]) -> Result<(), DevError> {
    if String::from_utf8_lossy(stdout)
        .lines()
        .any(|line| line.contains(r#""reason":"build-finished""#))
    {
        return Ok(());
    }
    Err(DevError::Build(format!(
        "cargo exited successfully but produced no recognisable artifact stream \
         (no build-finished record) for: cargo {}",
        args.join(" ")
    )))
}

/// The sweep's filters compiled onto the engine's own surfaces - the one
/// place a sweep's selection is translated for the engine, so preparation and
/// execution (which runs the prepared list) cannot disagree.
fn sweep_engine_filter(
    sweep: &ResolvedSweep,
    pcx: &ParseContext<'_>,
    known_groups: &nextest_filtering::KnownGroups,
) -> Result<TestFilter, DevError> {
    let mut patterns = TestFilterPatterns::new(sweep.name_filters.clone());
    let mut run_ignored = RunIgnored::default();
    let mut it = sweep.libtest_args.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--skip" => {
                let Some(v) = it.next() else {
                    return Err(DevError::Config("--skip without a value".into()));
                };
                patterns.add_skip_pattern(v.clone());
            }
            "--include-ignored" => run_ignored = RunIgnored::All,
            other => {
                return Err(DevError::Config(format!(
                    "sweep '{}' carries libtest arg `{other}`, which the nextest lane cannot \
                     translate; drop it from the profile, or use a libtest entry.",
                    sweep.label
                )));
            }
        }
    }
    // ONE filterset: the engine ORs the filtersets it is handed (a test runs if
    // any of them admits it), so one filterset per skip made disjoint skips
    // admit everything - `not A` admits what `not B` excludes. The skips are
    // subtractions that must all hold, so they are one conjunction.
    let mut filtersets: Vec<Filterset> = Vec::new();
    if let Some(expr) = qualified_skips_filterset(&sweep.qualified_skips)? {
        let parsed = Filterset::parse(expr.clone(), pcx, FiltersetKind::Test, known_groups).map_err(|e| {
            DevError::Config(format!("qualified skips `{expr}` did not compile: {e:?}"))
        })?;
        filtersets.push(parsed);
    }
    TestFilter::new(NextestRunMode::Test, run_ignored, patterns, filtersets)
        .map_err(|e| DevError::Config(format!("nextest test filter: {e}")))
}

/// Every package-qualified skip as one filterset: the conjunction of their
/// negations, `None` when there are none.
fn qualified_skips_filterset(skips: &[crate::config::QualifiedSkip]) -> Result<Option<String>, DevError> {
    let mut terms = Vec::with_capacity(skips.len());
    for qs in skips {
        terms.push(format!("({})", qualified_skip_filterset(qs)?));
    }
    Ok((!terms.is_empty()).then(|| terms.join(" & ")))
}

/// The filterset for one package-qualified skip: exclude tests matching the
/// pattern *within that package only*, exactly the semantics brokkr's own
/// evaluator gives the libtest lanes.
///
/// Values are interpolated into filterset source text, so the charset is
/// restricted to what a test path or package name can contain - anything
/// else is refused rather than escaped, because a wrongly-escaped filter
/// silently changes which tests run.
fn qualified_skip_filterset(qs: &crate::config::QualifiedSkip) -> Result<String, DevError> {
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-'))
    };
    if !ok(&qs.package) || !ok(&qs.pattern) {
        return Err(DevError::Config(format!(
            "qualified skip {{ package = \"{}\", pattern = \"{}\" }} contains characters the \
             nextest filterset translation does not interpolate; use a libtest entry for this \
             shape.",
            qs.package, qs.pattern
        )));
    }
    Ok(format!("not (package({}) & test(~{}))", qs.package, qs.pattern))
}

#[cfg(test)]
mod nextest_lane_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn artifact_stream_requires_build_finished() {
        let args = vec!["test".to_owned(), "--no-run".to_owned()];
        let incomplete = br#"{"reason":"compiler-artifact"}"#;
        let err = require_build_finished(incomplete, &args).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("no build-finished record"), "{message}");
        assert!(message.contains("cargo test --no-run"), "{message}");

        let complete = b"{\"reason\":\"compiler-artifact\"}\n{\"reason\":\"build-finished\",\"success\":true}\n";
        assert!(require_build_finished(complete, &args).is_ok());
    }

    /// The engine lane's preparation captures the runtime fingerprint from its
    /// own artifact stream - the one `prepare_nextest` stores on the lane -
    /// so a support bin another lane rebuilds since the plan is drift at the
    /// lane's verification. It used to discard the index and store `None`,
    /// and verification then never looked past the test executables.
    #[test]
    fn the_engine_lane_captures_its_runtime_fingerprint() {
        let dir = crate::test_scratch::scratch("nextest_lane", "runtime_fingerprint");
        let support = dir.join("servebin");
        std::fs::write(&support, b"planned server").unwrap();
        let exe = dir.join("suite-1");
        std::fs::write(&exe, b"suite").unwrap();
        let stdout = format!(
            "{}\n{}\n{}\n",
            format_args!(
                r#"{{"reason":"compiler-artifact","package_id":"path+file:///x/a#pkg-a@0.1.0","manifest_path":"/x/a/Cargo.toml","target":{{"name":"servebin","kind":["bin"]}},"profile":{{"test":false}},"executable":"{}"}}"#,
                support.display()
            ),
            format_args!(
                r#"{{"reason":"compiler-artifact","package_id":"path+file:///x/a#pkg-a@0.1.0","manifest_path":"/x/a/Cargo.toml","target":{{"name":"suite","kind":["test"]}},"profile":{{"test":true}},"executable":"{}"}}"#,
                exe.display()
            ),
            r#"{"reason":"build-finished","success":true}"#,
        );
        let art = nextest_lane_artifacts(&stdout).unwrap();
        let fp = art.runtime_fingerprint.clone().unwrap();
        assert!(fp.iter().any(|l| l.starts_with("bin ") && l.contains("servebin")), "{fp:?}");
        assert_eq!(art.binaries.len(), 1);

        // Through the lane's verification: the same stream verifies, and the
        // support bin rebuilt with other content is drift.
        let b = art.binaries[0].clone();
        let mut p = PreparedLane::bare(LaneEnv::default());
        p.runtime_fingerprint = art.runtime_fingerprint;
        p.resolutions.push(PreparedResolution {
            resolution: None,
            selection: Vec::new(),
            binaries: vec![PreparedBinary {
                unit: BinaryUnit::of(&b),
                hash: hash_file(Path::new(&b.executable)).unwrap(),
                envelope: None,
                unlisted: None,
                binary: b,
                selected: vec!["t".into()],
                ignored: BTreeSet::new(),
                benchmarks: Vec::new(),
            }],
        });
        let sweep = ResolvedSweep { label: "engine".into(), ..ResolvedSweep::default() };
        let rebuild = |_: &PreparedResolution| {
            let (bins, index, _) = parse_test_binaries(&stdout);
            Ok(Some((bins, index)))
        };
        verify_lane_with(&sweep, &p, rebuild, &[]).unwrap();
        std::fs::write(&support, b"another lane's server").unwrap();
        let err = verify_lane_with(&sweep, &p, rebuild, &[]).unwrap_err().to_string();
        assert!(err.contains("launch envelope"), "{err}");
    }

    // The sweep env must reach the engine's test processes, which only read
    // cargo's `[env]` table. Every entry is forced (a sweep value beats an
    // inherited one, as `Command::env` does on the libtest lanes), awkward
    // keys and values survive quoting, and the hold variables and the
    // orphan-reap token are left to the capability file alone.
    #[test]
    fn the_sweep_env_renders_as_a_forced_cargo_env_table() {
        let body = sweep_env_config_body(&[
            ("BROKKR_TEST_PLATFORM", "1"),
            ("WEIRD.KEY", "a \"quoted\" value"),
            (crate::hold::CAPABILITY_ENV, "stale"),
            (crate::test_orphans::MARKER_ENV, "stale"),
        ]);
        let parsed: toml::Table = toml::from_str(&body).unwrap();
        let env = parsed["env"].as_table().unwrap();
        let platform = env["BROKKR_TEST_PLATFORM"].as_table().unwrap();
        assert_eq!(platform["value"].as_str(), Some("1"));
        assert_eq!(platform["force"].as_bool(), Some(true));
        assert_eq!(platform["relative"].as_bool(), Some(false));
        assert_eq!(env["WEIRD.KEY"]["value"].as_str(), Some("a \"quoted\" value"));
        assert!(!env.contains_key(crate::hold::CAPABILITY_ENV), "{body}");
        assert!(!env.contains_key(crate::test_orphans::MARKER_ENV), "{body}");
    }

    /// The engine ORs the filtersets it is given (nextest-runner 0.126
    /// `BinaryFilter::check_match` folds them with `logic_or`), so disjoint
    /// qualified skips given one filterset each admit everything: a test
    /// excluded by one skip is admitted by the other's negation. They must be
    /// ONE filterset, the conjunction of the negations.
    #[test]
    fn qualified_skips_are_one_conjunction_of_negations() {
        use crate::config::QualifiedSkip;
        let skip = |package: &str, pattern: &str| QualifiedSkip { package: package.into(), pattern: pattern.into() };
        assert_eq!(qualified_skips_filterset(&[]).unwrap(), None);
        assert_eq!(
            qualified_skips_filterset(&[skip("a", "slow")]).unwrap().as_deref(),
            Some("(not (package(a) & test(~slow)))")
        );
        let both = qualified_skips_filterset(&[skip("a", "slow"), skip("b", "flaky")]).unwrap().unwrap();
        assert_eq!(both, "(not (package(a) & test(~slow))) & (not (package(b) & test(~flaky)))");
        assert!(!both.contains('|'), "no alternative: every skip must hold: {both}");
        // The interpolation rule still holds for every entry.
        assert!(qualified_skips_filterset(&[skip("a", "slow"), skip("b", "has space")]).is_err());
    }

    /// A recorded engine launch environment is compared line by line; any
    /// line that changed, vanished or appeared is a difference to refuse on.
    #[test]
    fn engine_launch_drift_names_what_changed() {
        let recorded = vec!["cargo-env {A}".to_owned(), "runner None".to_owned()];
        assert!(engine_launch_drift(&recorded, &recorded).is_empty());
        let now = vec!["cargo-env {A, B}".to_owned(), "runner None".to_owned()];
        let drift = engine_launch_drift(&recorded, &now);
        assert_eq!(drift.len(), 2, "{drift:?}");
        assert!(drift[0].contains("no longer holds") && drift[0].contains("{A}"), "{drift:?}");
        assert!(drift[1].contains("gained") && drift[1].contains("{A, B}"), "{drift:?}");
    }

    /// A one-package workspace laid out under `root`: its `cargo metadata`
    /// document, the artifact stream a build of its one test target would
    /// emit, and that target's executable - a shell script that lists two
    /// tests (`fast`, `slow`) and sleeps far past any ceiling when asked to
    /// run `slow`. Everything the engine reads, with no cargo involved.
    fn engine_fixture(root: &Path) -> (String, String) {
        use std::os::unix::fs::PermissionsExt as _;
        let pkg = root.join("core");
        let deps = root.join("target/debug/deps");
        std::fs::create_dir_all(pkg.join("tests")).unwrap();
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(
            pkg.join("Cargo.toml"),
            "[package]\nname = \"core\"\nversion = \"0.1.0\"\n\n[[test]]\nname = \"suite\"\n",
        )
        .unwrap();
        let exe = deps.join("suite-1");
        std::fs::write(
            &exe,
            "#!/bin/sh\n\
             case \"$*\" in\n\
             *--list*--ignored*) exit 0 ;;\n\
             *--list*) echo 'fast: test'; echo 'slow: test'; exit 0 ;;\n\
             esac\n\
             case \"$*\" in\n\
             *slow*) sleep 30 ;;\n\
             esac\n\
             exit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (root_s, id) = (root.display().to_string(), format!("path+file://{}/core#core@0.1.0", root.display()));
        let target = format!(
            r#"{{"kind":["test"],"crate_types":["bin"],"name":"suite","src_path":"{root_s}/core/tests/suite.rs","edition":"2021","doc":false,"doctest":false,"test":true}}"#
        );
        let metadata = format!(
            r#"{{"packages":[{{"name":"core","version":"0.1.0","id":"{id}","license":null,"license_file":null,"description":null,"source":null,"dependencies":[],"targets":[{target}],"features":{{}},"manifest_path":"{root_s}/core/Cargo.toml","metadata":null,"publish":null,"authors":[],"categories":[],"keywords":[],"readme":null,"repository":null,"homepage":null,"documentation":null,"edition":"2021","links":null,"default_run":null,"rust_version":null}}],"workspace_members":["{id}"],"workspace_default_members":["{id}"],"resolve":{{"nodes":[{{"id":"{id}","dependencies":[],"deps":[],"features":[]}}],"root":"{id}"}},"target_directory":"{root_s}/target","version":1,"workspace_root":"{root_s}/core","metadata":null}}"#
        );
        let stream = format!(
            "{}\n{}\n",
            format_args!(
                r#"{{"reason":"compiler-artifact","package_id":"{id}","manifest_path":"{root_s}/core/Cargo.toml","target":{target},"profile":{{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":true}},"features":[],"filenames":["{exe}"],"executable":"{exe}","fresh":true}}"#,
                exe = exe.display()
            ),
            r#"{"reason":"build-finished","success":true}"#,
        );
        (metadata, stream)
    }

    /// Wiring through the REAL engine: a test that blows the engine's
    /// per-test cap sets `EngineRun.timed_out` from the `TestFinished` events
    /// `run_nextest_engine` collects itself (nothing is injected), a lane
    /// whose tests finish does not, and the preparation that built the lane
    /// captured the engine's launch fingerprint through the real discovery
    /// path - which the lane's recipe carries and a replay compares.
    ///
    /// One test, run in sequence: engine preparation writes the per-host
    /// engine env file under `$HOME/.brokkr`, which two concurrent
    /// preparations would race on.
    #[test]
    fn a_real_engine_run_reports_its_timeout_and_the_launch_fingerprint() {
        let root = crate::test_scratch::scratch("nextest_lane", "real_engine");
        let (metadata, stream) = engine_fixture(&root);
        let inputs = LaneInputs {
            project: None,
            project_root: &root,
            state_root: &root,
            target_dir: &root,
            allow_flags: &[],
            commands: false,
            certifying: false,
        };
        let assemble = |sweep: &ResolvedSweep| {
            assemble_nextest_lane(
                &inputs,
                "engine",
                vec!["-p".into(), "core".into()],
                &LaneEnv::default(),
                &EngineFilter::Sweep(sweep),
                IgnoredMode::Exclude,
                &NextestBuild {
                    metadata_json: &metadata,
                    stdout: &stream,
                    build_platforms: host_build_platforms().unwrap(),
                    // One second: the cap the synthesized config carries.
                    ceiling: std::time::Duration::from_secs(1),
                },
            )
            .unwrap()
        };

        // Both tests listed; the slow one runs into the cap.
        let sweep = ResolvedSweep { label: "engine".into(), harness: crate::config::Harness::Nextest, ..ResolvedSweep::default() };
        let prepared = assemble(&sweep);
        assert_eq!(prepared.resolutions[0].binaries[0].selected, ["fast", "slow"]);
        let np = prepared.nextest.as_ref().expect("an engine lane");
        let tap = LaneTap::new(0);
        let run = run_nextest_engine("engine", Some(1), np, &tap).unwrap();
        assert!(run.timed_out, "the TestFinished event of the capped test sets timed_out");
        assert!(!run.passed);
        // The engine's cancellation after the timeout was recorded on the tap.
        assert!(tap.decisive_termination().is_some(), "the fail-fast stop was recorded");

        // A selection that never reaches the slow test finishes clean.
        let fast = ResolvedSweep { name_filters: vec!["fast".into()], ..sweep.clone() };
        let quick = assemble(&fast);
        let run = run_nextest_engine("engine", Some(1), quick.nextest.as_ref().unwrap(), &LaneTap::new(0)).unwrap();
        assert!(run.passed && !run.timed_out, "{run:?}");

        // The launch fingerprint came from the real discovery in preparation,
        // is what the lane's recipe records, and is what a replay compares.
        assert!(prepared.engine_launch.iter().any(|l| l.starts_with("cargo-env ")), "{:?}", prepared.engine_launch);
        assert!(prepared.engine_launch.iter().any(|l| l.starts_with("runner ")), "{:?}", prepared.engine_launch);
        let recipe = build_lane_replay(&sweep, &prepared).expect("an engine recipe");
        assert_eq!(recipe.engine_launch, prepared.engine_launch);
        assert!(engine_launch_drift(&recipe.engine_launch, &quick.engine_launch).is_empty());
        let mut changed = recipe.engine_launch.clone();
        changed.push("cargo-env {EXTRA}".into());
        assert!(!engine_launch_drift(&changed, &quick.engine_launch).is_empty());
    }

    /// The fail-fast stop signals every test still running. Those deaths are
    /// the cancellation's, so they read as interrupted - never passed, never
    /// a failure of the test - while an exit-code failure stays the test's
    /// own, and a timeout stays a timeout whatever the engine config says.
    #[test]
    fn engine_deaths_under_cancellation_are_interruptions() {
        use nextest_runner::config::elements::SlowTimeoutResult;
        use nextest_runner::reporter::events::AbortDescription;
        let killed = ExecutionResultDescription::Fail {
            failure: FailureDescription::Abort {
                abort: AbortDescription::UnixSignal { signal: 9, name: None },
            },
            leaked: false,
        };
        assert_eq!(engine_result(&killed, true), TestResult::Interrupted);
        assert_eq!(engine_result(&killed, false), TestResult::Failed);
        let exited = ExecutionResultDescription::Fail {
            failure: FailureDescription::ExitCode { code: 101 },
            leaked: false,
        };
        assert_eq!(engine_result(&exited, true), TestResult::Failed);
        let lenient = ExecutionResultDescription::Timeout { result: SlowTimeoutResult::Pass };
        assert_eq!(engine_result(&lenient, false), TestResult::TimedOut);
        assert_eq!(engine_result(&ExecutionResultDescription::Pass, true), TestResult::Ok);
        assert_eq!(cancel_cause(Some(CancelReason::TestFailureImmediate)), TerminationCause::FailFast);
        assert_eq!(cancel_cause(Some(CancelReason::ReportError)), TerminationCause::EngineError);
    }
}
