// The `harness = "nextest"` lane: the sweep is handed to the linked nextest
// engine, which owns build, list and run - process-per-test under the
// project's own `.config/nextest.toml`.
//
// Motivation is CI parity, and only parity (see docs/commands/check.md's
// nextest section): where a project's CI runs `cargo nextest run` and the
// suite relies on its per-test process isolation, brokkr's in-process libtest
// lanes exercise a shape CI never runs. This lane runs the real engine
// in-process - no `cargo-nextest` on PATH, the version pinned by brokkr's
// Cargo.lock.
//
// What brokkr keeps for itself:
// - the COMPILE SHAPE. The build is brokkr's cargo invocation (selection,
//   features, unification pin, cargo profile, [lints] allows, sweep env,
//   rustflags plumbing), streamed into nextest's BinaryListBuilder - so a
//   nextest lane and a libtest lane with equal compile inputs share the
//   target dir and the clippy dedupe.
// - the FILTERS. The sweep's `only`/`skip` map onto nextest's own
//   TestFilterPatterns (identical libtest substring semantics, no filterset
//   string to escape); a package-qualified skip becomes the filterset
//   `not (package(P) & test(~X))`, folding the one predicate brokkr used to
//   evaluate itself back into the tool.
// - the VERDICT and the bookends. Nextest's reporter renders the run; brokkr
//   wraps it in the usual sweep lines and decides pass/fail from RunStats.
//
// What nextest keeps: everything else. Its config's default profile (or
// NEXTEST_PROFILE), default-filter, retries, per-test timeouts, test groups,
// setup scripts. Brokkr deliberately imposes NO 20s watchdog here - the lane
// exists to run tests the way CI runs them - but a gate must be bounded, so
// after listing it verifies every selected test has a terminating timeout
// under the resolved profile and refuses otherwise, naming the config to fix
// (`slow-timeout = { period = "..", terminate-after = N }`).
//
// The linked engine's version is brokkr's pin, not the host's. It is
// recorded below and printed on the lane's header line, so a gate result
// says which engine produced it.

use camino::Utf8PathBuf;
use guppy::graph::PackageGraph;
use nextest_filtering::{Filterset, FiltersetKind, ParseContext};
use nextest_runner::{
    cargo_config::{CargoConfigs, EnvironmentMap},
    config::core::NextestConfig,
    double_spawn::DoubleSpawnInfo,
    input::InputHandlerKind,
    list::{
        BinaryListBuilder, ListProgressOptions, RustTestArtifact, TestExecuteContext, TestList,
    },
    platform::{BuildPlatforms, HostPlatform, PlatformLibdir},
    helpers::{ShowTerminalProgress, ThemeCharacters},
    reporter::{ReporterBuilder, ReporterOutput, ShowProgress, structured::StructuredReporter},
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
const NEXTEST_ENGINE_VERSION: &str = "0.122.1";

/// Run one `harness = "nextest"` sweep. Returns `Ok(false)` when the run
/// failed, having already reported it.
///
/// The wrapper exists to honour the test phase's reporting contract: a
/// failing phase prints its own detail (`cmd_check`'s error branch adds only
/// the timing line), so an `Err` leaving this lane must be voiced here or it
/// is silent.
#[allow(clippy::too_many_arguments)]
fn run_nextest_sweep(
    project_root: &Path,
    sweep: &ResolvedSweep,
    packages: &[&str],
    extra_args: &[String],
    project_env: &[(String, String)],
    allow_args: &[String],
    commands: bool,
) -> Result<bool, DevError> {
    run_nextest_sweep_inner(
        project_root, sweep, packages, extra_args, project_env, allow_args, commands,
    )
    .inspect_err(|e| output::error(&e.to_string()))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_nextest_sweep_inner(
    project_root: &Path,
    sweep: &ResolvedSweep,
    packages: &[&str],
    extra_args: &[String],
    project_env: &[(String, String)],
    allow_args: &[String],
    commands: bool,
) -> Result<bool, DevError> {
    let (cargo_extra, libtest_extra) = split_extra_args(extra_args);
    if !libtest_extra.is_empty() {
        return Err(DevError::Config(format!(
            "sweep '{}' runs under nextest, which does not take raw libtest args; drop the \
             trailing `-- {}` (use the sweep's `skip`/`only`, or nextest's own config).",
            sweep.label,
            libtest_extra.join(" ")
        )));
    }
    reject_unsupported_forwarded(sweep, cargo_extra)?;

    let env_full = merged_env(&sweep.env, project_env);
    let env_refs: Vec<(&str, &str)> = env_full
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    output::run_msg(&format!(
        "test {}: nextest engine {NEXTEST_ENGINE_VERSION}, process-per-test",
        sweep.label
    ));

    // The graph feeds config parsing (filterset predicates, test groups) and
    // artifact resolution. `--all-features --filter-platform` mirrors what
    // cargo-nextest itself asks for - the graph is a naming universe, not the
    // build's resolution.
    let host = HostPlatform::detect(PlatformLibdir::from_rustc_stdout(
        nextest_runner::RustcCli::print_host_libdir().read(),
    ))
    .map_err(|e| DevError::Build(format!("nextest host platform detection failed: {e}")))?;
    let triple = host.platform.triple_str().to_owned();
    let build_platforms = BuildPlatforms { host, target: None };

    let metadata = output::run_captured_with_env(
        "cargo",
        &[
            "metadata",
            "--format-version=1",
            "--all-features",
            "--filter-platform",
            &triple,
        ],
        project_root,
        &env_refs,
    )?;
    if !metadata.status.success() {
        output::error(&String::from_utf8_lossy(&metadata.stderr));
        return Err(DevError::Build("cargo metadata failed".into()));
    }
    let metadata_json = String::from_utf8_lossy(&metadata.stdout).into_owned();
    let graph = PackageGraph::from_json(&metadata_json)
        .map_err(|e| DevError::Build(format!("cargo metadata unparseable: {e}")))?;
    let workspace_root: Utf8PathBuf = graph.workspace().root().to_owned();

    let cargo_configs = CargoConfigs::new(Vec::<String>::new())
        .map_err(|e| DevError::Config(format!("cargo config discovery failed: {e}")))?;

    // The build is brokkr's: the same compile-shape argv every other lane
    // uses, streamed into nextest's builder so the artifact facts (binary
    // ids, build meta, dylib paths) are nextest's own reading of it.
    let mut args: Vec<String> = vec!["test".into(), "--no-run".into(), "--message-format=json".into()];
    args.extend(sweep_selection_args(sweep, packages));
    args.extend(allow_args.iter().cloned());
    args.extend(cargo_extra.iter().cloned());
    args.extend(sweep.unification_args());
    if let Some(p) = sweep.profile {
        args.extend(p.cargo_args().iter().map(|s| (*s).to_owned()));
    }
    if !has_target_selector(&args) {
        args.push("--tests".into());
    }
    if commands {
        output::run_msg(&format!("cargo {}", args.join(" ")));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let build = output::run_captured_with_env("cargo", &arg_refs, project_root, &env_refs)?;
    if !build.status.success() {
        output::error(&format!("failing command: cargo {}", args.join(" ")));
        output::error(&String::from_utf8_lossy(&build.stderr));
        return Ok(false);
    }
    let mut builder = BinaryListBuilder::new(&graph, build_platforms.clone());
    for line in String::from_utf8_lossy(&build.stdout).lines() {
        builder
            .process_message_line(line)
            .map_err(|e| DevError::Build(format!("nextest could not read the build: {e}")))?;
    }
    let binary_list = std::sync::Arc::new(builder.finish());

    // Nextest's own config, resolved exactly as `cargo nextest` would:
    // `.config/nextest.toml` under the workspace root, the default profile
    // unless NEXTEST_PROFILE says otherwise.
    let pcx = ParseContext::new(&graph);
    let config = NextestConfig::from_sources(
        workspace_root.clone(),
        &pcx,
        None,
        std::iter::empty::<&nextest_runner::config::core::ToolConfigFile>(),
        &std::collections::BTreeSet::new(),
    )
    .map_err(|e| DevError::Config(format!("nextest config: {e}")))?;
    let profile_name =
        std::env::var("NEXTEST_PROFILE").unwrap_or_else(|_| NextestConfig::DEFAULT_PROFILE.into());
    let early_profile = config
        .profile(&profile_name)
        .map_err(|e| DevError::Config(format!("nextest profile: {e}")))?;
    let known_groups = early_profile.known_groups();
    let profile = early_profile.apply_build_platforms(&build_platforms);

    // Filters: `only` and unqualified `skip` ride nextest's own libtest
    // pattern emulation (same substring semantics, nothing to escape);
    // package-qualified skips become one filterset each.
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
    let mut filtersets: Vec<Filterset> = Vec::new();
    for qs in &sweep.qualified_skips {
        let expr = qualified_skip_filterset(qs)?;
        let parsed = Filterset::parse(expr.clone(), &pcx, FiltersetKind::Test, &known_groups)
            .map_err(|e| {
                DevError::Config(format!("qualified skip `{expr}` did not compile: {e:?}"))
            })?;
        filtersets.push(parsed);
    }
    let test_filter = TestFilter::new(NextestRunMode::Test, run_ignored, patterns, filtersets)
        .map_err(|e| DevError::Config(format!("nextest test filter: {e}")))?;

    // Double-spawn re-invokes the CURRENT executable with a `__double-spawn`
    // subcommand - a protocol cargo-nextest's own binary implements and
    // brokkr does not, so enabling it makes every test command a brokkr clap
    // error. Disabled is a supported nextest mode (NEXTEST_DOUBLE_SPAWN=0);
    // the cost is a narrow unix signal race around spawn, not correctness of
    // results.
    let double_spawn = DoubleSpawnInfo::disabled();
    let target_runner =
        TargetRunner::new(&cargo_configs, &build_platforms).unwrap_or_else(|_| TargetRunner::empty());
    let run_id = nextest_runner::helpers::force_or_new_run_id();
    let version_env_vars = VersionEnvVars {
        current_version: NEXTEST_ENGINE_VERSION
            .parse()
            .map_err(|e| DevError::Build(format!("engine version constant: {e}")))?,
        required_version: None,
        recommended_version: None,
    };
    let ctx = TestExecuteContext {
        run_id,
        version_env_vars: &version_env_vars,
        profile_name: profile.name(),
        double_spawn: &double_spawn,
        target_runner: &target_runner,
    };

    let path_mapper = PathMapper::noop();
    let rust_build_meta = binary_list.rust_build_meta.map_paths(&path_mapper);
    let test_artifacts = RustTestArtifact::from_binary_list(
        &graph,
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
        workspace_root.clone(),
        env_map,
        &profile,
        FilterBound::DefaultSet,
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
            "cargo test: zero tests ran (sweep: {}) - the filters and the nextest profile's \
             default-filter selected no work; treat as a wrong-run.",
            sweep.label
        )));
    }

    verify_bounded(&profile, &test_list, &sweep.label)?;

    let runner = TestRunnerBuilder::default()
        .build(
            run_id,
            version_env_vars.clone(),
            &test_list,
            &profile,
            std::env::args().collect(),
            SignalHandlerKind::Standard,
            InputHandlerKind::Noop,
            double_spawn.clone(),
            target_runner.clone(),
        )
        .map_err(|e| DevError::Build(format!("nextest runner: {e}")))?;

    let mut reporter = ReporterBuilder::default().build(
        &test_list,
        &profile,
        ShowTerminalProgress::from_cargo_configs(&cargo_configs, false),
        ReporterOutput::Terminal,
        StructuredReporter::new(),
    );

    configure_handle_inheritance(false)
        .map_err(|e| DevError::Build(format!("nextest handle setup: {e}")))?;
    let run_stats = runner
        .try_execute(|event| reporter.report_event(event))
        .map_err(|e| DevError::Build(format!("nextest run failed to execute: {e}")))?;
    let _ = reporter.finish();

    let passed = run_stats.summarize_final();
    Ok(matches!(
        passed,
        nextest_runner::reporter::events::FinalRunStats::Success
    ))
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

/// A gate must be bounded, and this lane deliberately carries no watchdog of
/// its own - so every selected test must have a terminating timeout under
/// the resolved nextest profile. Checked per test AFTER listing, because a
/// profile-level `terminate-after` can be overridden away for a subset
/// (`[[profile.*.overrides]]` replacing the slow-timeout), and only the
/// per-test resolution sees that.
fn verify_bounded(
    profile: &nextest_runner::config::core::EvaluatableProfile<'_>,
    test_list: &TestList<'_>,
    label: &str,
) -> Result<(), DevError> {
    for test in test_list.iter_tests() {
        if !test.test_info.filter_match.is_match() {
            continue;
        }
        let query = test.to_test_query();
        let settings = profile.settings_for(NextestRunMode::Test, &query);
        // SlowTimeout's fields are crate-private with no accessor, so the
        // Debug rendering is the only readable surface. Failure direction is
        // safe: if an upgrade reshapes the Debug output this starts refusing
        // (loudly), never silently passing an unbounded lane.
        let rendered = format!("{:?}", settings.slow_timeout());
        if !rendered.contains("terminate_after: Some") {
            return Err(DevError::Config(format!(
                "sweep '{label}': test {} has no terminating timeout under nextest profile \
                 '{}' - a hang would run forever, and this lane deliberately adds no watchdog \
                 of its own. Set `slow-timeout = {{ period = \"60s\", terminate-after = N }}` \
                 in .config/nextest.toml (profile-wide or via an override).",
                test.id(),
                profile.name(),
            )));
        }
    }
    Ok(())
}
