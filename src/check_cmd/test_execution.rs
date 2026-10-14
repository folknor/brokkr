/// The feature-shape fragment of [`describe_sweep`], read back out of the
/// feature argv a request hands cargo (its projection, not the configured
/// list) so it can never drift from what cargo is actually handed.
fn describe_features(args: &[String]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--all-features" => parts.push("all-features".into()),
            "--no-default-features" => parts.push("no-default".into()),
            "--features" => {
                if let Some(list) = it.next() {
                    parts.push(format!("+{list}"));
                }
            }
            other => {
                if let Some(list) = other.strip_prefix("--features=") {
                    parts.push(format!("+{list}"));
                }
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// One-line human shape of a sweep: what distinguishes it from its siblings -
/// package scope, feature shape, rustflags - plus the test-phase-only bits
/// (libtest filters, thread policy) when `for_test`.
///
/// A green run never prints it: the shape is config plus invocation,
/// identical run to run. It goes to the run log and the status line, and a
/// failing sweep prints it beside its failing command. The full cargo command
/// is ~90% profile boilerplate repeated identically across every sweep (a
/// 14-entry `--skip` list dwarfs the `-p`/`--features` part that actually
/// varies), so the shape is the readable form of it. `rustflags` is always
/// surfaced here even though it is a single config field: it silently
/// redirects the sweep to an isolated target dir, and the log is where an
/// unexplained full recompile gets explained.
///
/// `selection` is what this run of the sweep selects (the phase's selection,
/// or one resolution of it), so the shape says what actually runs, not what
/// the config declares: an override names its packages, a one-package
/// selection names its package, a bare selection is cargo's default one - not
/// a claim about the workspace. `features` is the matching feature argv: one
/// request's projection, or for a lane-level line the tokens some run of the
/// lane carries ([`Attempt::described_features`]).
pub(crate) fn describe_sweep(
    sweep: &PlannedSweep,
    for_test: bool,
    selection: &Selection,
    features: &[String],
) -> String {
    let mut parts: Vec<String> = vec![match selection {
        Selection::Override { packages, .. } => {
            packages.iter().map(|pkg| format!("-p {pkg}")).collect::<Vec<_>>().join(" ")
        }
        Selection::Explicit(packages) => match packages.as_slice() {
            [one] => format!("-p {one}"),
            _ => output::count(packages.len(), "pkg"),
        },
        Selection::WorkspaceExcluding(excluded) => {
            format!("workspace -{}", output::count(excluded.len(), "pkg"))
        }
        Selection::Bare => "default selection".into(),
    }];

    // Suppress a feature fragment that just restates the label: the legacy
    // no-`[[check]]` path synthesizes a sweep literally named `all-features`,
    // and `clippy all-features: default selection, all-features` is noise.
    parts.extend(
        describe_features(features).filter(|feat| *feat != sweep.label),
    );

    if !sweep.rustflags.is_empty() {
        parts.push(format!(
            "rustflags {} (isolated target)",
            sweep.rustflags.join(" ")
        ));
    }

    // Surfaced for the same reason as `rustflags` below it: a pinned
    // resolution changes which features are on, so it compiles different code
    // - and package mode also sends the sweep to its own target dir. An
    // unexplained full rebuild is exactly what a collapsed log must not hide,
    // and the mode is the single most load-bearing fact about such a lane.
    // `auto` prints nothing: it is the status quo, and naming it on every
    // line of every ordinary run would be noise.
    if let Some(mode) = sweep.effective_unification().describe() {
        let isolated = if sweep.effective_unification().is_per_package() {
            " (isolated target)"
        } else {
            ""
        };
        parts.push(format!("unification {mode}{isolated}"));
    }

    // Surfaced for the same reason as `rustflags`: a non-default profile
    // sends the sweep to a different `target/` subdirectory and compiles it
    // from scratch there, and an unexplained full rebuild is exactly what a
    // collapsed log must not hide. Only when set - an unset profile is the
    // command's default and says nothing.
    if let Some(p) = sweep.profile {
        parts.push(format!("profile {}", p.target_subdir()));
    }

    if for_test {
        let filters = sweep.lane_filters();
        let execution = sweep.lane_exec();
        let skips = filters.skips().filter(|f| f.package.is_none()).count();
        if skips > 0 {
            parts.push(format!("{skips} skips"));
        }
        if filters.include_ignored {
            parts.push("include-ignored".into());
        }
        for target in &filters.targets {
            parts.push(format!("--test {}", target.as_str()));
        }
        for filter in filters.only() {
            parts.push(format!("filter {}", filter.pattern));
        }
        parts.push(
            if matches!(execution, crate::profile::LaneExec::DocOnly { .. }) {
                // Serial by construction, and the lane's whole identity: an
                // unexplained `--doc` invocation is the one thing a collapsed
                // log must not hide.
                "doc-only"
            } else if matches!(execution, crate::profile::LaneExec::Parallel { .. })
                || matches!(
                    execution,
                    crate::profile::LaneExec::Serial { threads } if threads.is_cargo_mediated()
                )
                || matches!(
                    execution,
                    crate::profile::LaneExec::Nextest { threads } if threads != Some(1)
                )
            {
                "parallel"
            } else {
                "serial"
            }
            .into(),
        );

        if matches!(execution, crate::profile::LaneExec::Isolated) {
            parts.push("process-isolated".into());
        }

        let package_skips = filters.skips().filter(|f| f.package.is_some()).count();
        if package_skips > 0 {
            parts.push(format!("{package_skips} pkg-skips"));
        }
    }

    parts.join(", ")
}

/// The `cargo build` argv for one `build_packages` pre-build.
///
/// The pre-build is one of the sweep's compiling paths, so it carries the
/// sweep's pinned unification like every other: the binaries the tests spawn
/// must come from the same feature graph as the tests themselves, or a
/// package-mode lane runs its tests against a binary built under ambient
/// resolution.
fn pre_build_args(sweep: &PlannedSweep, req: &SupportRequest, allow_args: &[String]) -> Vec<String> {
    // One JSON stream carries both the support executables and typed compiler
    // diagnostics. The message format does not enter cargo's fingerprint.
    let mut args: Vec<String> = vec!["build".into(), "--message-format=json".into()];
    args.extend(allow_args.iter().cloned());
    // The profile must match BROKKR_TEST_BIN_DIR, and the support request
    // carries only features its package can resolve.
    args.extend(support_selection_args(sweep, req, sweep.cargo_profile()));
    args
}

/// One executable a `build_packages` pre-build produced: what a test reaches
/// through `BROKKR_TEST_BIN_DIR`, which no test build's artifact stream names
/// when the package sits outside the test selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SupportArtifact {
    pub(crate) package: String,
    pub(crate) target: String,
    pub(crate) executable: String,
}

/// The executables in a pre-build's artifact stream (non-test artifacts that
/// carry an `executable`), attributed to the pre-built package.
pub(crate) fn support_artifacts(stdout: &str, package: &str) -> Vec<SupportArtifact> {
    #[derive(serde::Deserialize)]
    struct Artifact {
        reason: String,
        #[serde(default)]
        target: Option<Target>,
        #[serde(default)]
        executable: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Target {
        name: String,
        #[serde(default)]
        kind: Vec<String>,
    }
    let mut out: Vec<SupportArtifact> = Vec::new();
    for line in stdout.lines() {
        let Ok(a) = serde_json::from_str::<Artifact>(line) else { continue };
        if a.reason != "compiler-artifact" {
            continue;
        }
        let (Some(target), Some(executable)) = (a.target, a.executable) else { continue };
        // A dependency's build script is the build's machinery, not a
        // support executable anything at test time reads.
        if target.kind.iter().any(|k| k == "custom-build") {
            continue;
        }
        let art = SupportArtifact { package: package.to_owned(), target: target.name, executable };
        if !out.contains(&art) {
            out.push(art);
        }
    }
    out
}

/// The support executables' identities as fingerprint lines: package,
/// target, path and content hash, sorted. Hashed when called - after the
/// lane's own builds, which can re-uplift a support bin over the pre-build's,
/// so the file hashed is the one the lane's tests will read.
pub(crate) fn support_fingerprint(artifacts: &[SupportArtifact]) -> Result<Vec<String>, DevError> {
    let mut out = Vec::with_capacity(artifacts.len());
    for a in artifacts {
        let hash = crate::test_runner::hash_file(Path::new(&a.executable))
            .map_err(|e| DevError::Build(format!("could not hash support executable {}: {e}", a.executable)))?;
        out.push(format!("support {} {} {} {hash}", a.package, a.target, a.executable));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Build one binary package with the sweep's feature flags, returning the
/// executables it produced. Compiler messages and manifest lints use the same
/// structured diagnostic path as clippy and rustdoc.
fn run_sweep_pre_build(
    project_root: &Path,
    sweep: &PlannedSweep,
    req: &SupportRequest,
    project_env: &[(String, String)],
    allow_args: &[String],
    owners: &crate::diagnostic::OwnerIndex,
    commands: bool,
) -> Result<Vec<SupportArtifact>, DevError> {
    let package = req.package();
    let args = pre_build_args(sweep, req, allow_args);

    // A pre-build is part of its sweep's shape: logged, shown as the status,
    // printed only under `--commands`.
    let shape = format!("build {package} (sweep: {})", sweep.label);
    output::status(&shape);
    output::detail(&shape);
    cargo_line(
        commands,
        &format!("cargo {} (sweep build: {})", args.join(" "), sweep.label),
    );

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env_full = merged_env(&sweep.env, project_env);
    let env_refs: Vec<(&str, &str)> = env_full
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let captured = run_check_cargo(&arg_refs, project_root, &env_refs)?;

    let stdout = String::from_utf8_lossy(&captured.stdout);
    let stderr = String::from_utf8_lossy(&captured.stderr);
    let diagnostics = owned_build_diagnostics(&stdout, &stderr, owners);
    if captured.success() {
        if let Some(warnings) = render_build_diagnostics("cargo build", &diagnostics, true) {
            note_warning(&warnings, &sweep.label);
        }
        return Ok(support_artifacts(&stdout, package));
    }

    // Printed even under `--commands`: the streamed line is neither adjacent
    // to this failure nor attributable among several runs.
    // The identity leads and the error is `Reported`: nothing downstream
    // restates it, so this line is the only place the package and sweep appear
    // in the failure output.
    let failed = format!("build failed for package '{package}' in sweep '{}'", sweep.label);
    output::error(&failed);
    output::error(&format!("failing command: cargo {}", args.join(" ")));
    if let Some(rendered) = render_build_diagnostics("cargo build", &diagnostics, false) {
        output::error(&rendered);
    }
    let remaining_stderr = stderr_without_manifest_lints(&stderr);
    if !remaining_stderr.trim().is_empty() {
        output::error(remaining_stderr.trim());
    }
    Err(DevError::Reported(failed))
}

/// Why a serial sweep cannot isolate its harnesses, or `None` when it can.
///
/// The isolation installs brokkr as cargo's host target runner and rustdoc (see
/// `test_runner::harness_shim`), which is only safe when cargo would execute host
/// binaries directly: an override would silently drop a configured runner or
/// rustdoc, and a non-host target never reaches a host runner at all. Anything
/// ambiguous takes the first-failure path rather than guessing.
fn serial_shim_fallback(
    root: &Path,
    forwarded: &ForwardedArgs,
    project_env: &[(String, String)],
    sweep_env: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    // Cargo sees this merged environment. In particular, a sweep CARGO_HOME
    // changes the config chain the shim must inspect before replacing a runner.
    let cargo_env = merged_env(sweep_env, project_env);
    let cargo_env_refs: Vec<(&str, &str)> = cargo_env
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let launch = match crate::rustflags::CargoLaunchConfig::discover(
        root,
        crate::rustflags::EffectiveEnv::new(&cargo_env_refs),
    ) {
        Ok(launch) => launch,
        Err(error) => {
            return Some(format!("Cargo config could not be validated: {error}"));
        }
    };
    serial_shim_fallback_with_config(forwarded, &launch)
}

/// The snapshot variant used when lane setup already validated the Cargo
/// config chain. This keeps runner and rustdoc refusal on the same config view
/// the lane uses to reconstruct its launch environment.
pub(crate) fn serial_shim_fallback_with_config(
    forwarded: &ForwardedArgs,
    launch: &crate::rustflags::CargoLaunchConfig,
) -> Option<String> {
    if forwarded.changes_config_or_target() {
        return Some("forwarded --config or --target".into());
    }
    if let Some(target) = &launch.build_target {
        return Some(match &target.origin {
            crate::rustflags::SourceOrigin::Environment(_) => "CARGO_BUILD_TARGET is set".into(),
            crate::rustflags::SourceOrigin::Config(path) => {
                format!("{} sets build.target", path.display())
            }
        });
    }
    if let Some(rustdoc) = &launch.rustdoc {
        return Some(match &rustdoc.origin {
            crate::rustflags::SourceOrigin::Environment(_) => {
                "rustdoc executable is configured".into()
            }
            crate::rustflags::SourceOrigin::Config(path) => {
                format!("{} sets build.rustdoc", path.display())
            }
        });
    }
    // The runner execs brokkr through `/proc/<pid>/exe` (see
    // `harness_shim::own_exe`), so only a missing `/proc` rules it out.
    if std::fs::metadata("/proc/self/exe").is_err() {
        return Some("/proc/self/exe is unreadable, so cargo cannot exec brokkr as the runner".into());
    }
    if crate::rustflags::host_triple().is_none() {
        return Some("host target could not be determined".into());
    }
    if let Some(runner) = &launch.runner {
        return Some(match &runner.origin {
            crate::rustflags::SourceOrigin::Environment(_) => {
                format!("{} is set", runner.key)
            }
            crate::rustflags::SourceOrigin::Config(path) => {
                format!("{} configures a target runner", path.display())
            }
        });
    }
    None
}

/// Whether a cargo request carries the sweep's target selectors or only its
/// build shape. The latter leaves lane filters to the per-binary inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CargoSelectionPurpose {
    TestLane,
    Shape,
}

/// The cargo selection for one already planned resolution. This is the one
/// builder for profile, unification, package and projected feature flags; only
/// a test-lane request additionally carries the sweep's cargo target filters.
///
/// Every cargo run that compiles a sweep (clippy, rustdoc, the pre-build, the
/// test run, the per-binary reproduction and the coverage enumeration) goes
/// through here, because a profile or unification mismatch between any two
/// of them is a silent rebuild at best and lint results for a build that
/// never ran at worst.
pub(crate) fn cargo_selection_args(
    sweep: &PlannedSweep,
    run: &ResolutionRun,
    profile: crate::config::SweepProfile,
    purpose: CargoSelectionPurpose,
) -> Vec<String> {
    let mut args = profile
        .cargo_args()
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect::<Vec<_>>();
    args.extend(sweep.unification_args());
    args.extend(run.cargo_args());
    if purpose == CargoSelectionPurpose::TestLane {
        args.extend(sweep.lane_filters().cargo_args());
    }
    args
}

/// The profile, unification pin, and projected package request for one
/// `build_packages` support build. Both `check` and `brokkr test` use this
/// fragment so the support executable is built under the sweep's graph.
pub(crate) fn support_selection_args(
    sweep: &PlannedSweep,
    request: &SupportRequest,
    profile: crate::config::SweepProfile,
) -> Vec<String> {
    let mut args = profile
        .cargo_args()
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect::<Vec<_>>();
    args.extend(sweep.unification_args());
    args.extend(request.feature_args().iter().cloned());
    args.extend(request.package_args());
    args
}

/// One serial resolution's share of the plan: the executables its harnesses
/// may present, and the ones that must.
pub(crate) struct SerialPlan {
    pub(crate) resolution: Option<String>,
    /// Executable path -> unit: what a harness handshake resolves against.
    /// Only THIS lane and resolution's artifacts - a path planned for another
    /// lane does not satisfy it.
    pub(crate) units: HashMap<String, BinaryUnit>,
    /// Executable path -> planned content hash: the strict shim re-hashes a
    /// harness at its handshake, immediately before it runs.
    pub(crate) hashes: HashMap<String, String>,
    /// The planned binaries that hold expected executions: each must connect.
    pub(crate) expected: Vec<(String, BinaryUnit)>,
    /// This lane is a planned doctest carrier ([`lane_runs_doctests`]): only
    /// then may a rustdoc stream connect.
    pub(crate) rustdoc: bool,
    /// The run is certifying: the harness shim fails closed - a harness it
    /// cannot attribute to a planned binary, or whose content is not the
    /// planned one, is refused rather than run - and a planned harness that
    /// never connects is an attribution error. Otherwise the plan only
    /// attributes: the shim stays permissive, as it was before any lane had a
    /// plan, and an unknown harness is an unattributed stream, not a refusal.
    pub(crate) strict: bool,
}

/// How a serial run reports what it saw.
pub(crate) struct SerialObserve<'a> {
    pub(crate) tap: &'a LaneTap,
    /// `Some` where the lane has an inventory: harness streams attribute to
    /// its binaries. Strict (the shim fails closed) only when
    /// [`SerialPlan::strict`].
    pub(crate) plan: Option<SerialPlan>,
}

impl SerialObserve<'_> {
    /// The sink for this run: harness streams attributed by the executable
    /// their handshake named, rustdoc's to the doctest block, and cargo's own
    /// stream to nothing - test records there could not be attributed.
    fn sink(&self) -> crate::test_runner::ObservationSink {
        let resolution = self.plan.as_ref().and_then(|p| p.resolution.clone());
        let units = self.plan.as_ref().map(|p| p.units.clone()).unwrap_or_default();
        self.tap.sink(
            move |source| match source {
                crate::test_runner::StreamSource::Harness { executable } => match units.get(executable) {
                    Some(unit) => StreamOrigin::Binary {
                        resolution: resolution.clone(),
                        unit: unit.clone(),
                        one_test: None,
                    },
                    None => StreamOrigin::Unattributed { detail: format!("harness {executable}") },
                },
                crate::test_runner::StreamSource::Rustdoc => StreamOrigin::Doctest,
                crate::test_runner::StreamSource::Process => {
                    StreamOrigin::Unattributed { detail: "cargo's shared stream".into() }
                }
                // A serial lane runs no nextest engine; were one to report
                // here, nothing could attribute its stream to a binary.
                crate::test_runner::StreamSource::Engine => {
                    StreamOrigin::Unattributed { detail: "nextest engine stream".into() }
                }
            },
            true,
        )
    }

    /// After a cargo run that exited successfully: every planned harness with
    /// expected executions must have connected. Cargo saying it ran every
    /// binary while one never presented itself is an attribution failure, not
    /// a pass the plan can take on trust.
    fn missing_streams(&self) {
        let Some(plan) = self.plan.as_ref().filter(|p| p.strict) else {
            return;
        };
        let seen: BTreeSet<BinaryUnit> = self.tap.seen_units();
        for (exe, unit) in &plan.expected {
            if !seen.contains(unit) {
                self.tap.record(JournalRecord::Observed {
                    lane: self.tap.lane(),
                    stream: crate::test_runner::next_stream_id(),
                    origin: StreamOrigin::Unattributed { detail: format!("harness {exe}") },
                    event: crate::test_runner::ObsEvent::AttributionRefused {
                        refusal: crate::test_runner::AttributionRefusal::MissingPlannedStream {
                            executable: exe.clone(),
                            unit: unit.id(),
                        },
                    },
                });
            }
        }
    }
}

/// Whether this is the first time the process says a sweep ran without
/// harness isolation. Stated on a green run too: weaker isolation is part of
/// what the run was, and a failure would otherwise be the only place it shows.
fn first_isolation_notice(label: &str) -> bool {
    static SAID: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let Ok(mut said) = SAID.lock() else {
        return true;
    };
    if said.iter().any(|l| l == label) {
        return false;
    }
    said.push(label.to_owned());
    true
}

/// Run one cargo test invocation for the given sweep. Returns
/// `Ok(true)` on pass, `Ok(false)` on test failure (already reported),
/// `Err(...)` on subprocess spawn failure. `multi` controls whether
/// the `cargo ... (sweep: <label>)` log line carries the suffix - in
/// single-sweep mode (legacy `--all-features` path or one `[[check]]`
/// entry) the label noise is unhelpful.
#[allow(clippy::too_many_lines, clippy::too_many_arguments, clippy::cognitive_complexity)]
fn run_one_test_sweep(
    project_root: &Path,
    state_root: &Path,
    sweep: &PlannedSweep,
    run: &ResolutionRun,
    extra_args: &[String],
    project_env: &[(String, String)],
    owners: &crate::diagnostic::OwnerIndex,
    allow_args: &[String],
    doctests: bool,
    multi: bool,
    commands: bool,
    timings: Option<&mut Vec<TestTiming>>,
    observe: &SerialObserve<'_>,
) -> Result<bool, DevError> {
    let forwarded = ForwardedArgs::parse(extra_args);
    let (cargo_extra, libtest_extra) = (forwarded.cargo(), forwarded.libtest());
    // Cargo-mediated test threads take the unisolated runner; a watchdogged
    // serial lane isolates its harnesses unless it cannot.
    // (Under a complete profile neither escape exists: preparation refused
    // both, since neither can be attributed.)
    let execution = sweep.lane_exec();
    let parallel_threads = execution
        .threads()
        .is_some_and(crate::profile::Threads::is_cargo_mediated);
    let shim_reason = if parallel_threads {
        None
    } else {
        serial_shim_fallback(project_root, &forwarded, project_env, &sweep.env)
    };
    let use_shim = !parallel_threads && shim_reason.is_none();
    let sink = observe.sink();
    let obs = crate::test_runner::Observe {
        sink: Some(std::sync::Arc::clone(&sink)),
        shim: match (&observe.plan, use_shim) {
            (_, false) => crate::test_runner::ShimMode::Off,
            (Some(plan), true) if plan.strict => {
                crate::test_runner::ShimMode::Strict(crate::test_runner::StrictPlan {
                    known: plan.hashes.clone(),
                    rustdoc: plan.rustdoc,
                })
            }
            (_, true) => crate::test_runner::ShimMode::Permissive,
        },
    };

    let mut args: Vec<String> = vec!["test".into(), "--message-format=json".into()];
    // Before the selection and the `--` split: `--config` is a cargo option,
    // and everything past `--` belongs to libtest.
    args.extend(allow_args.iter().cloned());
    args.extend(cargo_selection_args(
        sweep,
        run,
        sweep.cargo_profile(),
        CargoSelectionPurpose::TestLane,
    ));
    // Without this, cargo stops after the FIRST test binary that fails, so a
    // red run reports the failures of one target and stays silent about every
    // later one. The exit code is honest either way, which is what made the
    // under-report invisible: an agent doing mutation verification reads the
    // list, sees one entry, and concludes the wrong test carried the coverage.
    // `brokkr check` is a whole-tree gate - it must enumerate every failure it
    // can reach in one run. Skipped when the caller already asked for it.
    //
    // But only with harness isolation. Without it every harness shares one
    // libtest stream, a harness that dies mid-test leaves its suite open, and
    // the next harness would be billed for the dead test - so a sweep that
    // cannot isolate stops at the first failing harness instead, and says so
    // below. Dropping a forwarded --no-fail-fast too, for the same reason.
    if shim_reason.is_none() && !cargo_extra.iter().any(|c| c == "--no-fail-fast") {
        args.push("--no-fail-fast".into());
    }
    for c in cargo_extra {
        if shim_reason.is_some() && c == "--no-fail-fast" {
            continue;
        }
        args.push(c.clone());
    }
    // A doc-only sweep runs doctests and nothing else - `--doc` is cargo's
    // exclusive doctest pseudo-target, and forwarded/configured selectors
    // were refused up front, so nothing can widen it here. Otherwise,
    // exclude doctests unless the project opted in (`[test] doctests =
    // true`): nextest - and therefore every brokkr-managed project's CI -
    // never runs doctests, so running them here is a signal CI can't see.
    // `--tests` selects lib + bins + integration but not doctests. A sweep
    // that already carries an explicit target selector (e.g. a profile's
    // `--test <name>`) excludes doctests on its own, and `--tests` would
    // wrongly broaden it, so only inject when no selector is present.
    if matches!(execution, crate::profile::LaneExec::DocOnly { .. }) {
        args.push("--doc".into());
    } else if !doctests
        && sweep.lane_filters().targets.is_empty()
        && !forwarded.has_target_selector()
    {
        args.push("--tests".into());
    }

    // Thread policy comes from the resolved lane; every policy uses the
    // same JSON tracker and hard per-test watchdog.
    let parallel = parallel_threads;
    let filters = sweep.lane_filters();
    let mut libtest_args = filters.libtest_args();
    libtest_args.extend(filters.name_filters());
    if parallel {
        // The default count leaves the flag off; an explicit count sets it.
        if let Some(threads) = execution.threads()
            && let Some(n) = threads.configured_count()
            && n > 1
        {
            libtest_args.push(format!("--test-threads={n}"));
        }
    } else if test_runner::effective_test_threads(&libtest_args)?.is_none() {
        libtest_args.push("--test-threads=1".into());
    }
    test_runner::LibtestArgs::validate_passthrough(libtest_extra)?;
    libtest_args.extend(libtest_extra.iter().cloned());
    // The runner owns JSON framing and renders it after the selection.

    let needs_separator = !libtest_args.is_empty();
    if needs_separator {
        args.push("--".into());
        for arg in &libtest_args {
            args.push(arg.clone());
        }
    }

    let command = if multi {
        format!("cargo {} (sweep: {})", args.join(" "), sweep.label)
    } else {
        format!("cargo {}", args.join(" "))
    };
    announce_sweep(
        &format!("test {}: {}", sweep.label, describe_sweep(sweep, true, &run.selection, run.feature_args())),
        Some(&command),
        commands,
    );
    // Once per sweep: a package-mode sweep resolves once per package, and the
    // reason is a property of the sweep's launch shape, not of a resolution.
    if let Some(reason) = &shim_reason
        && first_isolation_notice(&sweep.label)
    {
        output::warn(&format!(
            "test {}: harness isolation unavailable ({reason}); this run stops at the first \
             failing test harness",
            sweep.label
        ));
    }
    // What the grouped test line and a zero-test note call this execution
    // unit: the sweep, qualified by its package when one sweep runs as
    // several resolutions, or by the CLI `-p` set that replaced its selection.
    let unit = match (&run.resolution, &run.selection) {
        (Some(pkg), _) => format!("{} ({pkg})", sweep.label),
        (None, Selection::Override { packages, .. }) => format!("{} ({})", sweep.label, packages.join(", ")),
        (None, _) => sweep.label.clone(),
    };

    // Reprinted on any failure below, `--commands` or not: when a sweep fails,
    // the copy-pasteable cargo line is the most useful thing in the output, and
    // one streamed earlier is neither beside the failure nor attributable.
    let full_command = format!("failing command: cargo {}", args.join(" "));

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env_full = merged_env(&sweep.env, project_env);
    let env_refs: Vec<(&str, &str)> = env_full
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // One runner for both thread policies. Many tests share one cargo process
    // either way, so the per-test clock can only name a suspect and the wall
    // clock is what actually bounds the sweep. One cargo invocation, so there
    // are no siblings to cancel.
    let built_suffix = if parallel { " (parallel)" } else { "" };
    let libtest_run = test_runner::run_libtest(
        test_runner::Launch::Cargo,
        test_runner::LibtestInvocation {
            args: test_runner::LibtestArgs::from_argv(test_runner::Launch::Cargo, &arg_refs)?,
            cwd: project_root,
            state_root,
            env: &env_refs,
            context: test_runner::RunContext::process(),
            abort: None,
        },
        test_runner::Ceilings::shared_harness(),
        &obs,
        |_| {},
        |_| {},
        {
            let unit = unit.clone();
            move |elapsed| built_line(&unit, elapsed, built_suffix)
        },
    )?;
    // The typed verdict: a stop is the run's, and a failure the observations
    // record (a failed test, a protocol issue) fails the sweep even when
    // cargo's own exit said otherwise.
    let verdict = test_runner::lane_verdict(&libtest_run.outcome);
    if verdict == test_runner::LaneVerdict::Stopped {
        return Err(DevError::Interrupted);
    }
    let test_runner::LibtestRun { captured, hung, completed, .. } = libtest_run;

    if let Some(out) = timings {
        for (name, elapsed) in completed {
            out.push(TestTiming {
                sweep: sweep.label.clone(),
                name,
                elapsed,
            });
        }
    }

    let stdout = String::from_utf8_lossy(&captured.stdout);
    let stderr = String::from_utf8_lossy(&captured.stderr);

    // A blown time budget stops brokkr; it does not merely fail a sweep.
    //
    // Returning `Ok(false)` here let the phase carry on to the next package
    // resolution and the next sweep, which is right for a *failing test* - the
    // phase exists to report every failure it can reach - and wrong for a
    // timeout. A test that burned its budget means the run is already outside the
    // contract every later measurement assumes, and whatever wedged it (a
    // deadlock, a lock nobody will release, a runaway child) is still there for
    // the next sweep to inherit. `Err` propagates out of the sweep loop and ends
    // the run.
    if let Some(hung) = hung {
        // The sweep leads: `check` does not echo the `Reported` label below,
        // and neither the hung-test block nor the cargo line names it.
        output::error(&format!(
            "a test execution exceeded its {}s deadline in sweep '{}' - stopping",
            hung.ceiling.as_secs(),
            sweep.label
        ));
        output::error(&test_runner::format_hung_test(&hung, project_root));
        output::error(&full_command);
        return Err(DevError::Reported(format!(
            "a test execution exceeded its {}s deadline in sweep '{}' - stopping",
            hung.ceiling.as_secs(),
            sweep.label
        )));
    }

    let diagnostics = owned_build_diagnostics(&stdout, &stderr, owners);
    if !captured.success() || verdict == test_runner::LaneVerdict::Fail {
        output::error(&full_command);
        let has_test_summary = stdout.lines().any(|line| line.starts_with("test result:"));
        let has_compile_error = diagnostics.iter().any(crate::diagnostic::Diagnostic::is_error);
        if !has_test_summary && has_compile_error {
            if let Some(rendered) = render_build_diagnostics("cargo test", &diagnostics, false) {
                output::error(&rendered);
            }
            let remaining_stderr = stderr_without_manifest_lints(&stderr);
            if !remaining_stderr.trim().is_empty() {
                output::error(remaining_stderr.trim());
            }
        } else {
            output::error(&crate::test_runner::filter_test(&stdout, &stderr));
        }
        return Ok(false);
    }
    observe.missing_streams();

    // A green sweep prints no test output. To watch one test run, run it:
    // `brokkr test <NAME>` streams its stdout/stderr live. A gate is not a
    // log tail, and the build warnings below are the one thing here that a
    // passing run still needs to say.
    // Held until the phase ends and merged with the identical block every other
    // sweep produced: one cargo warning seen by four sweeps is one warning.
    if let Some(warnings) = render_build_diagnostics("cargo test", &diagnostics, true) {
        note_warning(&warnings, &sweep.label);
    }

    // Successful exit, but a profile/filter combo could still have
    // collected zero tests - cargo exits 0 with `0 passed; 0 failed`
    // and the user thinks `brokkr check` validated something. Fail
    // loudly when at least one suite ran but every test was filtered
    // out, since that's the silent-wrong-run shape. Suites = 0 (parse
    // failure) is also fatal: we can't tell what cargo did.
    let stdout_lines: Vec<&str> = stdout.lines().collect();
    let parsed = crate::test_runner::parse_test_output(&stdout_lines);
    if zero_test_run(&parsed) {
        let label = if multi {
            format!(" (sweep: {})", sweep.label)
        } else {
            String::new()
        };
        output::error(&format!(
            "cargo test: zero tests ran{label} ({}, {} filtered out) - \
             a profile/filter combo collected no work; treat as a wrong-run.",
            output::count(parsed.suites, "suite"),
            parsed.filtered_out,
        ));
        output::error(&full_command);
        return Ok(false);
    }

    // A stream that stopped describing itself cannot green a sweep. `zero_test_run`
    // deliberately returns false for an incomplete stream - blaming the filter for
    // a crash would be wrong - which left the incomplete case with no check of its
    // own, so a run like `running 1 test` / `running 1 test` / one summary, exit 0,
    // printed "1 passed" and returned green while a whole suite went unreported.
    if let crate::test_runner::Completeness::Incomplete { reason } = &parsed.completeness {
        let label = if multi {
            format!(" (sweep: {})", sweep.label)
        } else {
            String::new()
        };
        output::error(&format!(
            "cargo test: the test stream did not finish reporting{label}: {reason}. Observed \
             {} passed, {} failed, {} ignored. The process exited successfully, so this is a \
             harness that stopped talking rather than a failing test - treat as a wrong-run.",
            parsed.passed, parsed.failed, parsed.ignored
        ));
        output::error(&full_command);
        return Ok(false);
    }

    // The symmetric close to "running tests" above: always account for how
    // many tests actually ran, 0 or thousands - into the phase's grouped test
    // line, and per unit into the run log. On a green run every counted test
    // passed (a failure returns early). The wrong-run shapes were already
    // caught by `zero_test_run`, so a 0 here is a *legitimate* empty run, and
    // the grouped line names it rather than letting it vanish into the total;
    // on an explicit `-p` spot-check it also earns a warning, since `--tests`
    // excludes doctests and an all-doctest crate greens on clippy alone.
    note_tests(parsed.passed, parsed.ignored, parsed.filtered_out);
    output::detail(&format!(
        "test {unit}: {} passed, {} ignored, {} filtered out",
        parsed.passed, parsed.ignored, parsed.filtered_out
    ));
    if parsed.passed == 0 {
        let why = if parsed.ignored > 0 {
            format!("{unit} (all {} ignored)", parsed.ignored)
        } else {
            unit.clone()
        };
        note_empty_unit(why);
    }
    let total = parsed.passed + parsed.failed + parsed.ignored;
    let spot_check = run.resolution.is_some() || matches!(run.selection, Selection::Override { .. });
    if total == 0 && spot_check {
        let flags: Vec<String> =
            run.selection.packages().unwrap_or_default().iter().map(|p| format!("-p {p}")).collect();
        output::warn(&format!(
            "`{}` ran no tests - clippy passed, but nothing was validated \
             (doctests are excluded; the tests may be doctests or live in another crate)",
            flags.join(" "),
        ));
    }
    Ok(true)
}

/// The build-finished callback of a test run: the split between compile time
/// and test time, for the log and the status line.
fn built_line(unit: &str, elapsed: std::time::Duration, lane: &str) {
    let msg = format!(
        "test {unit}: test binaries built in {:.1}s; running tests{lane}",
        elapsed.as_secs_f64()
    );
    output::detail(&msg);
    output::status(&msg);
}

/// True when a successful `cargo test` run actually validated nothing.
///
/// Two distinct shapes:
/// - `suites == 0`: parser found no `test result:` line at all (cargo
///   succeeded but emitted unexpected output, or all suites were
///   filtered out by `--test cli_x` matching nothing). Treat as fatal:
///   we can't tell what ran.
/// - `passed + failed + ignored == 0` while at least one suite ran and
///   `filtered_out > 0`: every test in the matched suites was excluded
///   by the libtest filter (`--skip` / positional name). The user
///   thinks they tested something; they didn't.
///
/// A suite that legitimately defines zero tests (`#[cfg(test)] mod`
/// with no `#[test]`s) prints `running 0 tests` + `0 filtered out` and
/// is *not* flagged - that's a real, if empty, run.
fn zero_test_run(p: &crate::test_runner::ParsedTestResults) -> bool {
    if p.suites == 0 {
        return true;
    }
    // A truncated stream is not a zero-test run - it is a run that stopped
    // reporting, and its zeros mean "never said" rather than "nothing matched".
    // Calling it a wrong-run would blame the filter for a crash; the caller's
    // exit-status and timeout paths own that case.
    if !p.is_complete() {
        return false;
    }
    p.accounted() == 0 && p.filtered_out > 0
}

