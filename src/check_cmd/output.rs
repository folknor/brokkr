/// Project + profile-aware env vars set on every cargo test/build
/// subprocess. Owned `String`s (B7: avoids borrow-from-local traps the
/// previous `Vec<(&str, &str)>` shape allowed) and absolute paths
/// (B8: `CARGO_TARGET_TMPDIR` was relative `target/tmp`, fragile if
/// the cargo subprocess ever ran with a different cwd).
///
/// `target_dir` is cargo's resolved `target_directory` (from
/// `cargo metadata --no-deps`) - workspaces can place it outside the
/// project root, so the caller passes it in rather than us assuming
/// `<project_root>/target`. `profile_dir` is the profile name as it
/// appears under that target: `"debug"` for `brokkr check` (which
/// always tests in the dev profile) and `brokkr test --debug`,
/// `"release"` for the default `brokkr test` invocation.
pub(crate) fn build_test_env(
    project: Option<Project>,
    target_dir: &Path,
    profile_dir: &str,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if matches!(project, Some(Project::Nidhogg)) {
        let tmp = target_dir.join("tmp");
        out.push((
            "CARGO_TARGET_TMPDIR".into(),
            tmp.to_string_lossy().into_owned(),
        ));
    }
    let bin_dir = target_dir.join(profile_dir);
    out.push((
        "BROKKR_TEST_BIN_DIR".into(),
        bin_dir.to_string_lossy().into_owned(),
    ));
    out
}

/// The isolated target dir a sweep's `rustflags` imply, or `None` when it sets
/// none. Keyed on the flag content (`<target>/rustflags-<hash>`), so every sweep
/// carrying identical flags shares one cache and a global cfg change (e.g.
/// `--cfg madsim`) never thrashes the plain sweeps' shared target dir.
///
/// `meta_target_dir` is cargo's resolved `target_directory` (from `cargo
/// metadata`), not `<project_root>/target`: a workspace `.cargo/config.toml`
/// `[build] target-dir` can place it on another drive entirely, and the
/// isolated dir must sit beside the real one so `brokkr clean` and the plain
/// sweeps find it (S3-20).
fn isolated_target_dir(sweep: &ResolvedSweep, meta_target_dir: &Path) -> Option<std::path::PathBuf> {
    crate::config::rustflags_target_key(&sweep.rustflags)
        .map(|key| meta_target_dir.join(format!("rustflags-{key}")))
}

/// Compose a sweep's `rustflags` with any inherited flags into the env pair to
/// export. Appends to an inherited `CARGO_ENCODED_RUSTFLAGS` (0x1f-separated)
/// when present - cargo ignores `RUSTFLAGS` once the encoded form is set - else
/// to `RUSTFLAGS` (space-joined, matching `make cargo-test-sim`). `None` for an
/// empty flag list.
fn composed_rustflags_env(rustflags: &[String]) -> Option<(String, String)> {
    if rustflags.is_empty() {
        return None;
    }
    if let Ok(existing) = std::env::var("CARGO_ENCODED_RUSTFLAGS") {
        let mut parts: Vec<String> = if existing.is_empty() {
            Vec::new()
        } else {
            existing.split('\u{1f}').map(str::to_owned).collect()
        };
        parts.extend(rustflags.iter().cloned());
        return Some(("CARGO_ENCODED_RUSTFLAGS".into(), parts.join("\u{1f}")));
    }
    let mut flags = std::env::var("RUSTFLAGS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_default();
    if !flags.is_empty() {
        flags.push(' ');
    }
    flags.push_str(&rustflags.join(" "));
    Some(("RUSTFLAGS".into(), flags))
}

/// The `CARGO_TARGET_DIR` + `RUSTFLAGS` pair a sweep's `rustflags` imply, or an
/// empty vec when it sets none. Used by the clippy phase (which needs no
/// `BROKKR_TEST_BIN_DIR`); the test phase gets the same knobs plus the bin dir
/// through [`sweep_runtime_env`].
pub(crate) fn sweep_cargo_env(
    sweep: &ResolvedSweep,
    meta_target_dir: &Path,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(dir) = isolated_target_dir(sweep, meta_target_dir) {
        out.push(("CARGO_TARGET_DIR".into(), dir.to_string_lossy().into_owned()));
    }
    out.extend(composed_rustflags_env(&sweep.rustflags));
    out
}

/// The full base env for one sweep's test/pre-build cargo runs: `build_test_env`
/// computed against the sweep's *effective* target dir (its isolated
/// `target/rustflags-<hash>` when it carries `rustflags`, else cargo's own
/// `meta_target_dir`), plus the `CARGO_TARGET_DIR` / `RUSTFLAGS` overlay. The
/// sweep's own `env` still overlays this via `merged_env`.
pub(crate) fn sweep_runtime_env(
    sweep: &ResolvedSweep,
    project: Option<Project>,
    meta_target_dir: &Path,
    profile_dir: &str,
) -> Vec<(String, String)> {
    let isolated = isolated_target_dir(sweep, meta_target_dir);
    let effective: &Path = isolated.as_deref().unwrap_or(meta_target_dir);

    let mut out = build_test_env(project, effective, profile_dir);
    if let Some(dir) = &isolated {
        out.push(("CARGO_TARGET_DIR".into(), dir.to_string_lossy().into_owned()));
    }
    out.extend(composed_rustflags_env(&sweep.rustflags));
    out
}

/// The feature-shape fragment of [`describe_sweep`], read back out of the
/// already-flattened `cargo_feature_args` so it can never drift from what
/// cargo is actually handed.
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
/// This is the routine success form. The full cargo command is ~90% profile
/// boilerplate repeated identically across every sweep (a 14-entry `--skip`
/// list dwarfs the `-p`/`--features` part that actually varies), so it is
/// reprinted verbatim only when a sweep fails, or on demand via
/// `brokkr check --commands`. `rustflags` is always surfaced here even though
/// it is a single config field: it silently redirects the sweep to an isolated
/// target dir, and an unexplained full recompile is exactly the thing a
/// collapsed log must not hide.
/// Resolve the CLI `-p` flag against one sweep's own package selection.
///
/// Cargo *unions* package-selection flags: `--workspace --exclude a -p X`
/// selects the whole workspace minus `a` (the `-p` is silently swallowed),
/// and `-p a -p X` selects both. So a CLI `-p` must *replace* the sweep's
/// selection, never combine with it - and a package outside the sweep's
/// declared scope skips the sweep (mirroring `brokkr test`'s SKIP) rather
/// than force-running a selection the sweep's config rules out. Returns
/// `Err(reason)` when the sweep should be skipped.
pub(crate) fn cli_package_scope<'a>(
    sweep: &ResolvedSweep,
    package: Option<&'a str>,
    for_test: bool,
) -> Result<Option<&'a str>, String> {
    let Some(pkg) = package else {
        return Ok(None);
    };
    if for_test && sweep.test_exclude_packages.iter().any(|e| e == pkg) {
        return Err(format!(
            "-p {pkg} is in this sweep's test_exclude_packages"
        ));
    }
    if !sweep.packages.is_empty() && !sweep.packages.iter().any(|p| p == pkg) {
        return Err(format!("-p {pkg} is not in this sweep's packages list"));
    }
    Ok(Some(pkg))
}

pub(crate) fn describe_sweep(
    sweep: &ResolvedSweep,
    for_test: bool,
    cli_package: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(pkg) = cli_package {
        // A CLI `-p` replaced the sweep's own selection (cli_package_scope);
        // the shape must say what actually runs, not what the config declares.
        parts.push(format!("-p {pkg}"));
    } else if !sweep.packages.is_empty() {
        parts.push(format!("{} pkgs", sweep.packages.len()));
    } else if for_test && !sweep.test_exclude_packages.is_empty() {
        parts.push(format!(
            "workspace -{} pkgs",
            sweep.test_exclude_packages.len()
        ));
    } else {
        parts.push("workspace".into());
    }

    // Suppress a feature fragment that just restates the label: the legacy
    // no-`[[check]]` path synthesizes a sweep literally named `all-features`,
    // and `clippy all-features: workspace, all-features` is noise.
    parts.extend(
        describe_features(&sweep.cargo_feature_args).filter(|feat| *feat != sweep.label),
    );

    if !sweep.rustflags.is_empty() {
        parts.push(format!(
            "rustflags {} (isolated target)",
            sweep.rustflags.join(" ")
        ));
    }

    if for_test {
        let skips = sweep.libtest_args.iter().filter(|a| *a == "--skip").count();
        if skips > 0 {
            parts.push(format!("{skips} skips"));
        }
        if sweep.libtest_args.iter().any(|a| a == "--include-ignored") {
            parts.push("include-ignored".into());
        }
        // `cargo_test_filters` is stored flattened as `["--test", name, ...]`;
        // pair each flag back with its name so one filter reads as one item,
        // not `--test` and the bare name as two comma-separated fragments.
        let mut filters = sweep.cargo_test_filters.iter();
        while let Some(flag) = filters.next() {
            match filters.next() {
                Some(name) => parts.push(format!("{flag} {name}")),
                None => parts.push(flag.clone()),
            }
        }
        for name in &sweep.name_filters {
            parts.push(format!("filter {name}"));
        }
        parts.push(
            if matches!(sweep.test_threads, Some(n) if n != 1) {
                "parallel"
            } else {
                "serial"
            }
            .into(),
        );

        if sweep.process_isolation {
            parts.push("process-isolated".into());
        }

        if !sweep.qualified_skips.is_empty() {
            parts.push(format!("{} pkg-skips", sweep.qualified_skips.len()));
        }
    }

    parts.join(", ")
}

/// The log line announcing one sweep's cargo run: the full command under
/// `--commands`, else `<phase> <label>: <shape>`.
pub(crate) fn sweep_run_line(
    phase: &str,
    sweep: &ResolvedSweep,
    args: &[String],
    for_test: bool,
    commands: bool,
    cli_package: Option<&str>,
) -> String {
    if commands {
        return format!("cargo {}", args.join(" "));
    }
    format!(
        "{phase} {}: {}",
        sweep.label,
        describe_sweep(sweep, for_test, cli_package)
    )
}

/// Build one binary package with the sweep's feature flags. Errors
/// surface compile failures the same way the test phase does: filter
/// the stderr through `cargo_filter::filter_clippy` (or pass it
/// through raw).
fn run_sweep_pre_build(
    project_root: &Path,
    sweep: &ResolvedSweep,
    package: &str,
    project_env: &[(String, String)],
    raw: bool,
    commands: bool,
) -> Result<(), DevError> {
    let mut args: Vec<String> = vec!["build".into()];
    for f in &sweep.cargo_feature_args {
        args.push(f.clone());
    }
    args.push("--package".into());
    args.push(package.into());

    if commands {
        output::run_msg(&format!(
            "cargo {} (sweep build: {})",
            args.join(" "),
            sweep.label
        ));
    } else {
        output::run_msg(&format!("build {package} (sweep: {})", sweep.label));
    }

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env_full = merged_env(&sweep.env, project_env);
    let env_refs: Vec<(&str, &str)> = env_full
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, &env_refs)?;

    if captured.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&captured.stderr);
    if !commands {
        output::error(&format!("failing command: cargo {}", args.join(" ")));
    }
    if raw {
        if !stderr.is_empty() {
            output::error(&stderr);
        }
    } else {
        output::error(&cargo_filter::filter_clippy(&stderr));
    }
    Err(DevError::Build(format!(
        "build failed for package '{package}' in sweep '{}'",
        sweep.label
    )))
}

/// True if the cargo-section args already name a build target, in which
/// case cargo's default "everything incl. doctests" selection is off and
/// doctests are already excluded. Any explicit `--test`/`--tests`/`--lib`/
/// `--bin(s)`/`--example(s)`/`--bench(es)` selector (with or without an
/// `=value`) counts; the caller uses this to avoid appending `--tests` on
/// top of a `--test <name>` scope. `--test-threads` never appears here (it
/// is a libtest flag emitted after `--`).
fn has_target_selector(args: &[String]) -> bool {
    args.iter().any(|a| {
        a.starts_with("--test")
            || a.starts_with("--lib")
            || a.starts_with("--bin")
            || a.starts_with("--example")
            || a.starts_with("--bench")
            || a.starts_with("--doc")
            || a.starts_with("--all-targets")
    })
}

/// The cargo-level selection + feature args shared by the standard and
/// process-isolated test paths, so the two can never diverge on what a
/// sweep selects. A CLI `-p` *replaces* the sweep's package selection -
/// cargo unions selection flags, so emitting `--workspace --exclude …
/// --package X` would silently run the whole workspace (see
/// cli_package_scope; callers already skipped sweeps whose config rules
/// the package out).
fn sweep_selection_args(sweep: &ResolvedSweep, package: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();

    if let Some(pkg) = package {
        args.push("--package".into());
        args.push(pkg.into());
    } else {
        // Scope to the sweep's packages (`-p <pkg>`) so `--features` is
        // valid in a virtual workspace, mirroring the clippy phase.
        for pkg in &sweep.packages {
            args.push("-p".into());
            args.push(pkg.clone());
        }
        // Or, exclude packages from the whole workspace (test phase only).
        // Parse rejects setting both `packages` and `test_exclude_packages`,
        // so these two loops never both emit. `--exclude` requires
        // `--workspace`.
        if !sweep.test_exclude_packages.is_empty() {
            args.push("--workspace".into());
            for pkg in &sweep.test_exclude_packages {
                args.push("--exclude".into());
                args.push(pkg.clone());
            }
        }
    }
    for f in &sweep.cargo_feature_args {
        args.push(f.clone());
    }
    for f in &sweep.cargo_test_filters {
        args.push(f.clone());
    }
    args
}

/// Run one cargo test invocation for the given sweep. Returns
/// `Ok(true)` on pass, `Ok(false)` on test failure (already reported),
/// `Err(...)` on subprocess spawn failure. `multi` controls whether
/// the `cargo ... (sweep: <label>)` log line carries the suffix - in
/// single-sweep mode (legacy `--all-features` path or one [[check]]
/// entry) the label noise is unhelpful.
#[allow(clippy::too_many_lines, clippy::too_many_arguments, clippy::cognitive_complexity)]
fn run_one_test_sweep(
    project_root: &Path,
    state_root: &Path,
    sweep: &ResolvedSweep,
    package: Option<&str>,
    extra_args: &[String],
    project_env: &[(String, String)],
    raw: bool,
    doctests: bool,
    multi: bool,
    commands: bool,
    timings: Option<&mut Vec<TestTiming>>,
) -> Result<bool, DevError> {
    let (cargo_extra, libtest_extra) = split_extra_args(extra_args);

    let mut args: Vec<String> = vec!["test".into()];
    args.extend(sweep_selection_args(sweep, package));
    for c in cargo_extra {
        args.push(c.clone());
    }
    // Exclude doctests unless the project opted in (`[test] doctests = true`).
    // nextest - and therefore every brokkr-managed project's CI - never runs
    // doctests, so running them here is a signal CI can't see. `--tests`
    // selects lib + bins + integration but not doctests. A sweep that already
    // carries an explicit target selector (e.g. a profile's `--test <name>`)
    // excludes doctests on its own, and `--tests` would wrongly broaden it, so
    // only inject when no selector is present.
    if !doctests && !has_target_selector(&args) {
        args.push("--tests".into());
    }

    // Thread policy. A serial sweep (`test_threads` unset or 1) runs under the
    // per-test hang watchdog, which requires `--test-threads=1`. A sweep whose
    // profile set `test_threads` to 0 (libtest default parallelism) or >=2 runs
    // in parallel - no watchdog, one whole-sweep timeout instead.
    let parallel = matches!(sweep.test_threads, Some(n) if n != 1);

    let mut libtest_args = Vec::new();
    for s in &sweep.libtest_args {
        libtest_args.push(s.clone());
    }
    for n in &sweep.name_filters {
        libtest_args.push(n.clone());
    }
    if parallel {
        // Some(0) leaves the flag off (libtest default); Some(n>=2) sets n.
        if let Some(n) = sweep.test_threads
            && n >= 2
        {
            libtest_args.push(format!("--test-threads={n}"));
        }
        // Drive libtest's JSON event stream so the per-test hang watchdog can
        // age each in-flight test even under concurrency (human output emits no
        // per-test *start* signal in parallel). Native on nightly.
        if libtest_args.iter().any(|a| a == "--format") {
            return Err(DevError::Config(
                "a parallel test sweep drives libtest's JSON output for the per-test \
                 watchdog; remove the `--format` override from this profile's \
                 libtest_args".into(),
            ));
        }
        libtest_args.push("-Z".into());
        libtest_args.push("unstable-options".into());
        libtest_args.push("--format".into());
        libtest_args.push("json".into());
    } else if test_runner::effective_test_threads(&libtest_args)?.is_none() {
        libtest_args.push("--test-threads=1".into());
    }
    for e in libtest_extra {
        libtest_args.push(e.clone());
    }
    if !parallel && test_runner::effective_test_threads(&libtest_args)? != Some(1) {
        return Err(DevError::Config(
            "brokkr check watchdog requires --test-threads=1; set `test_threads` in \
             the profile to run this sweep in parallel, or drop the --test-threads \
             override".into(),
        ));
    }

    let needs_separator = !libtest_args.is_empty();
    if needs_separator {
        args.push("--".into());
        for arg in &libtest_args {
            args.push(arg.clone());
        }
    }

    let line = if commands && multi {
        format!("cargo {} (sweep: {})", args.join(" "), sweep.label)
    } else {
        sweep_run_line("test", sweep, &args, true, commands, package)
    };
    output::run_msg(&line);

    // Reprinted on any failure below: when a sweep fails, the copy-pasteable
    // cargo line is the most useful thing in the output, so collapsing applies
    // to success only.
    let full_command = format!("failing command: cargo {}", args.join(" "));

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env_full = merged_env(&sweep.env, project_env);
    let env_refs: Vec<(&str, &str)> = env_full
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // Serial => the watchdog runner; parallel => the whole-sweep-timeout
    // runner. Both reduce to (captured, optional hung test, timed_out, per-test
    // timings) so the reporting below is shared.
    let (captured, hung, timed_out, completed) = if parallel {
        let run = test_runner::run_libtest_parallel(
            &arg_refs,
            project_root,
            state_root,
            &env_refs,
            test_runner::PARALLEL_SWEEP_TIMEOUT,
            test_runner::TEST_TIMEOUT,
            |_| {},
            |_| {},
            move |elapsed| {
                println!(
                    "[test]    test binaries built in {:.1}s; running tests (parallel)",
                    elapsed.as_secs_f64()
                );
            },
        )?;
        let hung = match run.outcome {
            LibtestOutcome::HungTest(h) => Some(h),
            LibtestOutcome::Completed => None,
        };
        (run.captured, hung, run.timed_out, run.completed)
    } else {
        let run = test_runner::streaming_run_libtest(
            &arg_refs,
            project_root,
            state_root,
            &env_refs,
            test_runner::TEST_TIMEOUT,
            |_| {},
            |_| {},
            move |elapsed| {
                println!(
                    "[test]    test binaries built in {:.1}s; running tests",
                    elapsed.as_secs_f64()
                );
            },
        )?;
        let hung = match run.outcome {
            LibtestOutcome::HungTest(h) => Some(h),
            LibtestOutcome::Completed => None,
        };
        (run.captured, hung, false, run.completed)
    };

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

    if timed_out {
        output::error(&format!(
            "sweep '{}' exceeded the parallel test timeout ({}s) and was killed",
            sweep.label,
            test_runner::PARALLEL_SWEEP_TIMEOUT.as_secs(),
        ));
        if !commands {
            output::error(&full_command);
        }
        return Ok(false);
    }

    if let Some(hung) = hung {
        output::error(&test_runner::format_hung_test(&hung, project_root));
        if !commands {
            output::error(&full_command);
        }
        return Ok(false);
    }

    if !captured.status.success() {
        if !commands {
            output::error(&full_command);
        }
        if raw {
            if !stderr.is_empty() {
                output::error(&stderr);
            }
            if !stdout.is_empty() {
                output::error(&stdout);
            }
        } else {
            output::error(&cargo_filter::filter_test(&stdout, &stderr));
        }
        return Ok(false);
    }

    if raw {
        if !stderr.is_empty() {
            print!("{stderr}");
        }
        if !stdout.is_empty() {
            print!("{stdout}");
        }
    } else {
        let filtered = cargo_filter::filter_clippy(&stderr);
        if filtered != "cargo clippy: no issues" {
            let relabeled = filtered.replacen("cargo clippy:", "cargo test:", 1);
            output::warn(&relabeled);
        }
    }

    // Successful exit, but a profile/filter combo could still have
    // collected zero tests - cargo exits 0 with `0 passed; 0 failed`
    // and the user thinks `brokkr check` validated something. Fail
    // loudly when at least one suite ran but every test was filtered
    // out, since that's the silent-wrong-run shape. Suites = 0 (parse
    // failure) is also fatal: we can't tell what cargo did.
    let stdout_lines: Vec<&str> = stdout.lines().collect();
    let parsed = cargo_filter::parse_test_output(&stdout_lines);
    if zero_test_run(&parsed) {
        let label = if multi {
            format!(" (sweep: {})", sweep.label)
        } else {
            String::new()
        };
        output::error(&format!(
            "cargo test: zero tests ran{label} ({} suite(s), {} filtered out) - \
             a profile/filter combo collected no work; treat as a wrong-run.",
            parsed.suites, parsed.filtered_out,
        ));
        if !commands {
            output::error(&full_command);
        }
        return Ok(false);
    }

    // The symmetric close to "running tests" above: always report how many
    // tests actually ran, 0 or thousands. On a green run every counted test
    // passed (a failure returns early), so the headline is the pass count;
    // ignored / filtered-out are appended only when non-zero. The wrong-run
    // shapes were already caught by `zero_test_run`, so a 0 here is a
    // *legitimate* empty run - but on an explicit `-p` spot-check it is worth
    // a word, since `--tests` excludes doctests and an all-doctest crate
    // greens on clippy alone.
    let label = if multi {
        format!(" (sweep: {})", sweep.label)
    } else {
        String::new()
    };
    let mut extra = String::new();
    if parsed.ignored > 0 {
        extra.push_str(&format!(", {} ignored", parsed.ignored));
    }
    if parsed.filtered_out > 0 {
        extra.push_str(&format!(", {} filtered out", parsed.filtered_out));
    }
    println!("[test]    {} passed{extra}{label}", parsed.passed);
    let total = parsed.passed + parsed.failed + parsed.ignored;
    if total == 0
        && let Some(pkg) = package
    {
        output::warn(&format!(
            "`-p {pkg}` ran no tests - clippy passed, but nothing was validated \
             (doctests are excluded; its tests may be doctests or live in another crate)",
        ));
    }
    Ok(true)
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
fn zero_test_run(p: &cargo_filter::ParsedTestResults) -> bool {
    if p.suites == 0 {
        return true;
    }
    let total = p.passed + p.failed + p.ignored;
    total == 0 && p.filtered_out > 0
}

/// Combine the sweep's profile-defined env with the project's
/// always-set vars (e.g. nidhogg's `CARGO_TARGET_TMPDIR`). Sweep
/// values come first; project values append (so a sweep can shadow a
/// project default if it really needs to).
pub(crate) fn merged_env(
    sweep_env: &std::collections::BTreeMap<String, String>,
    project_env: &[(String, String)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> =
        sweep_env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in project_env {
        if !out.iter().any(|(ek, _)| ek == k) {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::useless_vec
    )]
    use super::*;

    #[test]
    fn decide_active_sweeps_legacy_default_when_nothing_configured() {
        let sweeps = decide_active_sweeps(&[], None, None, &[], false).unwrap();
        assert_eq!(sweeps.len(), 1);
        // The legacy-fallback label tracks the cargo flag so callers
        // that need to recognize this branch don't have to compare
        // feature-arg vectors. Don't change without updating
        // `brokkr test` (it relied on this distinction pre-fix).
        assert_eq!(sweeps[0].label, "all-features");
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--all-features"]);
        assert!(sweeps[0].build_packages.is_empty());
        assert!(sweeps[0].libtest_args.is_empty());
    }

    #[test]
    fn decide_active_sweeps_cli_features_create_ad_hoc() {
        // --features commands → single ad-hoc sweep, ignores `[[check]]`
        // and any profile entirely.
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: vec!["pbfhogg-cli".into()],
            ..Default::default()
        }];
        let sweeps = decide_active_sweeps(
            &entries,
            None,
            None,
            &["commands".to_owned()],
            false,
        )
        .unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "default");
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--features", "commands"]);
        // No build_packages on ad-hoc - the user is spot-checking.
        assert!(sweeps[0].build_packages.is_empty());
    }

    #[test]
    fn decide_active_sweeps_no_default_features_alone_is_ad_hoc() {
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        let sweeps = decide_active_sweeps(&entries, None, None, &[], true).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "default");
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--no-default-features"]);
    }

    #[test]
    fn decide_active_sweeps_check_entries_no_profile() {
        let entries = vec![
            CheckEntry {
                name: "all".into(),
                features: vec!["a".into(), "b".into()],
                no_default_features: false,
                build_packages: vec!["pbfhogg-cli".into()],
                ..Default::default()
            },
            CheckEntry {
                name: "consumer".into(),
                features: vec!["commands".into()],
                no_default_features: true,
                build_packages: vec!["pbfhogg-cli".into()],
                ..Default::default()
            },
        ];
        let sweeps = decide_active_sweeps(&entries, None, None, &[], false).unwrap();
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "all");
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--features", "a,b"]);
        assert_eq!(sweeps[0].build_packages, vec!["pbfhogg-cli"]);
        assert!(sweeps[0].libtest_args.is_empty());
        assert_eq!(sweeps[1].label, "consumer");
    }

    #[test]
    fn decide_active_sweeps_default_profile_when_no_explicit() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]
skip = ["tier2::"]
include_ignored = false
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: vec!["pbfhogg-cli".into()],
            ..Default::default()
        }];
        let sweeps =
            decide_active_sweeps(&entries, Some(&test_cfg), None, &[], false).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "all");
        assert_eq!(sweeps[0].libtest_args, vec!["--skip", "tier2::"]);
    }

    #[test]
    fn decide_active_sweeps_explicit_profile_overrides_default() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]

[profiles.full]
sweeps = ["all"]
include_ignored = true
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        let sweeps =
            decide_active_sweeps(&entries, Some(&test_cfg), Some("full"), &[], false).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert!(sweeps[0].libtest_args.contains(&"--include-ignored".into()));
    }

    #[test]
    fn decide_active_sweeps_profile_without_test_section_errors() {
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        let err = decide_active_sweeps(&entries, None, Some("tier1"), &[], false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--profile tier1"), "got: {err}");
    }

    #[test]
    fn sweep_tag_formats() {
        assert_eq!(sweep_tag(&[], 1), None);
        assert_eq!(sweep_tag(&["consumer".into()], 2), Some("[consumer]".into()));
        // Hit in both of two active sweeps -> `[both]` is honest.
        assert_eq!(
            sweep_tag(&["all-features".into(), "consumer".into()], 2),
            Some("[both]".into())
        );
    }

    #[test]
    fn sweep_tag_avoids_both_when_more_than_two_sweeps_active() {
        // B5: `[both]` with three active sweeps would hide which two
        // actually triggered the hit. Fall through to the explicit
        // joined form so the reader sees the real pair.
        assert_eq!(
            sweep_tag(&["a".into(), "b".into()], 3),
            Some("[a+b]".into())
        );
    }

    #[test]
    fn merge_clippy_dedups_and_combines_sweeps() {
        let stderr_a = "\
warning: x [unused_variables]
 --> src/foo.rs:1:1
  |
warning: y [needless_pass_by_value]
 --> src/bar.rs:2:1
  |
";
        let stderr_b = "\
warning: x [unused_variables]
 --> src/foo.rs:1:1
  |
warning: z [too_many_lines]
 --> src/baz.rs:3:1
  |
";
        let parses = vec![
            ("all-features".to_owned(), cargo_filter::parse_clippy(stderr_a)),
            ("consumer".to_owned(), cargo_filter::parse_clippy(stderr_b)),
        ];
        let merged = merge_clippy(&parses);
        // 3 unique diagnostics: foo (both), bar (a), baz (b).
        assert_eq!(merged.len(), 3);
        let foo = merged
            .iter()
            .find(|m| m.diag.path().unwrap().to_string_lossy().contains("foo.rs"))
            .unwrap();
        assert_eq!(foo.sweeps, vec!["all-features", "consumer"]);
        let bar = merged
            .iter()
            .find(|m| m.diag.path().unwrap().to_string_lossy().contains("bar.rs"))
            .unwrap();
        assert_eq!(bar.sweeps, vec!["all-features"]);
        let baz = merged
            .iter()
            .find(|m| m.diag.path().unwrap().to_string_lossy().contains("baz.rs"))
            .unwrap();
        assert_eq!(baz.sweeps, vec!["consumer"]);
    }

    fn json_compiler_message(
        level: &str,
        code: Option<&str>,
        message: &str,
        file: &str,
        line: u64,
        col: u64,
    ) -> String {
        let code_field = match code {
            Some(c) => format!(r#""code":{{"code":"{c}"}},"#),
            None => "\"code\":null,".to_string(),
        };
        format!(
            r#"{{"reason":"compiler-message","message":{{{code_field}"level":"{level}","message":"{message}","spans":[{{"file_name":"{file}","line_start":{line},"column_start":{col},"line_end":{line},"column_end":{col},"is_primary":true}}],"children":[],"rendered":"rendered"}}}}"#
        )
    }

    #[test]
    fn json_to_clippy_uses_code_for_every_occurrence() {
        // Regression: in cargo's pretty-printed text, only the first
        // occurrence of each lint per crate carries a `= note: #[warn(rule)]`
        // line, so the old text scraper left subsequent warnings as bare
        // `warning`. With JSON ingestion every diagnostic carries
        // `message.code.code`, so they all keep the rule in the header.
        let mut input = json_compiler_message(
            "warning",
            Some("clippy::collapsible_if"),
            "this `if` statement can be collapsed",
            "src/compose.rs",
            219,
            9,
        );
        input.push('\n');
        input.push_str(&json_compiler_message(
            "warning",
            Some("clippy::collapsible_if"),
            "this `if` statement can be collapsed",
            "src/compose.rs",
            228,
            9,
        ));

        let parsed = parse_clippy_from_json(&input, false, false);
        assert!(!parsed.parse_failed);
        assert_eq!(parsed.diagnostics.len(), 2);
        for d in &parsed.diagnostics {
            assert_eq!(d.header, "warning[clippy::collapsible_if]");
        }
    }

    #[test]
    fn json_to_clippy_uses_primary_label_for_detail() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"file_name":"src/foo.rs","line_start":20,"column_start":5,"line_end":20,"column_end":10,"is_primary":true,"label":"expected `i32`, found `&str`"}],"children":[],"rendered":"rendered"}}"#;
        let parsed = parse_clippy_from_json(input, false, false);
        assert_eq!(parsed.diagnostics.len(), 1);
        let d = &parsed.diagnostics[0];
        assert_eq!(d.header, "error[E0308]");
        assert_eq!(
            d.format_one(),
            "error[E0308] src/foo.rs:20:5 mismatched types - expected `i32`, found `&str`"
        );
    }

    #[test]
    fn json_to_clippy_falls_back_to_child_note_for_detail() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"file_name":"src/lib.rs","line_start":42,"column_start":12,"line_end":42,"column_end":15,"is_primary":true,"label":"arguments to this function are incorrect"}],"children":[{"level":"note","message":"expected reference `&Vec<u8>`\n   found reference `&Vec<i32>`","spans":[]}],"rendered":"rendered"}}"#;
        let parsed = parse_clippy_from_json(input, false, false);
        assert_eq!(parsed.diagnostics.len(), 1);
        let d = &parsed.diagnostics[0];
        assert!(
            d.format_one()
                .contains("- expected reference `&Vec<u8>`, found reference `&Vec<i32>`"),
            "got: {}",
            d.format_one()
        );
    }

    #[test]
    fn json_to_clippy_no_code_falls_back_to_bare_level() {
        // Some diagnostics lack a code (e.g. cargo-emitted notes). The
        // header degrades gracefully to bare `warning` / `error`.
        let input = json_compiler_message(
            "warning",
            None,
            "something happened",
            "src/foo.rs",
            10,
            5,
        );
        let parsed = parse_clippy_from_json(&input, false, false);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(parsed.diagnostics[0].header, "warning");
    }

    #[test]
    fn json_to_clippy_orders_errors_before_warnings() {
        let mut input = json_compiler_message(
            "warning",
            Some("clippy::redundant_closure"),
            "redundant closure",
            "src/a.rs",
            1,
            1,
        );
        input.push('\n');
        input.push_str(&json_compiler_message(
            "error",
            Some("E0308"),
            "mismatched types",
            "src/b.rs",
            2,
            2,
        ));
        let parsed = parse_clippy_from_json(&input, false, false);
        assert_eq!(parsed.diagnostics.len(), 2);
        assert!(parsed.diagnostics[0].is_error);
        assert!(!parsed.diagnostics[1].is_error);
    }

    #[test]
    fn gate_promotes_capped_warning_to_error() {
        // Under `--cap-lints=warn` a deny lint arrives at `warning` level; the
        // gate restores it to `error` for both the flag and the header, so
        // brokkr counts and displays it as the failure it is.
        let input = json_compiler_message(
            "warning",
            Some("clippy::manual_string_new"),
            "empty String is being created manually",
            "src/a.rs",
            3,
            5,
        );
        let parsed = parse_clippy_from_json(&input, false, true);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert!(parsed.diagnostics[0].is_error);
        assert_eq!(
            parsed.diagnostics[0].header,
            "error[clippy::manual_string_new]"
        );
    }

    #[test]
    fn json_to_clippy_sets_parse_failed_when_sweep_failed_with_no_events() {
        // cargo crashed before producing any compiler-message events.
        let parsed = parse_clippy_from_json("", true, false);
        assert!(parsed.parse_failed);
        assert!(parsed.diagnostics.is_empty());
    }

    #[test]
    fn json_to_clippy_no_parse_failed_when_sweep_succeeded() {
        // Empty stdout but successful exit (clean compile). Not a parse
        // failure - just nothing to report.
        let parsed = parse_clippy_from_json("", false, false);
        assert!(!parsed.parse_failed);
        assert!(parsed.diagnostics.is_empty());
    }

    fn diag(header: &str, location: &str) -> cargo_filter::ClippyDiagnostic {
        cargo_filter::ClippyDiagnostic {
            is_error: header.starts_with("error"),
            header: header.to_string(),
            location: Some(location.to_string()),
            message: "msg".to_string(),
            detail: None,
        }
    }

    #[test]
    fn clippy_sort_key_orders_errors_before_warnings() {
        let warn = diag("warning[clippy::aaaa]", "src/a.rs:1:1");
        let err = diag("error[E0308]", "src/z.rs:99:99");
        assert!(clippy_sort_key(&err) < clippy_sort_key(&warn));
    }

    #[test]
    fn clippy_sort_key_groups_same_lint_together() {
        // Three warnings - two with the same lint code on different files,
        // one with a different code in between alphabetically. After sort,
        // the same-lint pair should be adjacent.
        let mut diags = vec![
            diag("warning[clippy::collapsible_if]", "src/b.rs:1:1"),
            diag("warning[clippy::needless_return]", "src/a.rs:1:1"),
            diag("warning[clippy::collapsible_if]", "src/a.rs:1:1"),
        ];
        diags.sort_by_cached_key(clippy_sort_key);
        assert_eq!(diags[0].header, "warning[clippy::collapsible_if]");
        assert_eq!(diags[1].header, "warning[clippy::collapsible_if]");
        assert_eq!(diags[2].header, "warning[clippy::needless_return]");
        // Within the same lint, file order kicks in: a.rs before b.rs.
        assert_eq!(diags[0].location.as_deref(), Some("src/a.rs:1:1"));
        assert_eq!(diags[1].location.as_deref(), Some("src/b.rs:1:1"));
    }

    #[test]
    fn clippy_sort_key_orders_lines_numerically() {
        // Same lint, same file: line 9 before line 100 (lexical sort
        // would put 100 first - check we're parsing the integer).
        let mut diags = vec![
            diag("warning[clippy::xxx]", "src/a.rs:100:1"),
            diag("warning[clippy::xxx]", "src/a.rs:9:1"),
        ];
        diags.sort_by_cached_key(clippy_sort_key);
        assert_eq!(diags[0].location.as_deref(), Some("src/a.rs:9:1"));
        assert_eq!(diags[1].location.as_deref(), Some("src/a.rs:100:1"));
    }

    #[test]
    fn clippy_sort_key_pushes_bare_level_to_end() {
        // A bare `warning` (no code) should sort after every coded
        // warning, since there's no useful key to group it with.
        let mut diags = vec![
            diag("warning", "src/a.rs:1:1"),
            diag("warning[clippy::zzz]", "src/b.rs:1:1"),
            diag("warning[clippy::aaa]", "src/c.rs:1:1"),
        ];
        diags.sort_by_cached_key(clippy_sort_key);
        assert_eq!(diags[0].header, "warning[clippy::aaa]");
        assert_eq!(diags[1].header, "warning[clippy::zzz]");
        assert_eq!(diags[2].header, "warning");
    }

    #[test]
    fn parse_location_handles_normal_path_line_col() {
        assert_eq!(
            parse_location(Some("src/foo.rs:10:5")),
            ("src/foo.rs".to_string(), 10, 5)
        );
    }

    #[test]
    fn parse_location_handles_none() {
        assert_eq!(parse_location(None), (String::new(), 0, 0));
    }

    #[test]
    fn extract_lint_code_pulls_bracketed_name() {
        assert_eq!(extract_lint_code("warning[clippy::foo]"), "clippy::foo");
        assert_eq!(extract_lint_code("error[E0308]"), "E0308");
        assert_eq!(extract_lint_code("warning"), "");
    }

    fn s(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn split_extra_args_no_separator_all_cargo_level() {
        // `brokkr check -- --test read_paths` (clap consumed the leading
        // `--`, leaving us with the two tokens). Cargo gets `--test
        // read_paths`; nothing crosses to libtest.
        let extra = s(&["--test", "read_paths"]);
        let (cargo, libtest) = split_extra_args(&extra);
        assert_eq!(cargo, &["--test", "read_paths"]);
        assert!(libtest.is_empty());
    }

    #[test]
    fn split_extra_args_double_dash_routes_to_libtest() {
        // `brokkr check -- -- --ignored`: clap consumed the first `--`,
        // we see `["--", "--ignored"]`. The literal `--` we observe is
        // the *cargo/libtest* boundary - everything after it is libtest.
        let extra = s(&["--", "--ignored"]);
        let (cargo, libtest) = split_extra_args(&extra);
        assert!(cargo.is_empty());
        assert_eq!(libtest, &["--ignored"]);
    }

    #[test]
    fn split_extra_args_mixed_form_routes_each_side() {
        // `brokkr check -- --test cli -- --ignored --nocapture`: cargo
        // gets the test filter, libtest gets the runtime flags.
        let extra = s(&["--test", "cli", "--", "--ignored", "--nocapture"]);
        let (cargo, libtest) = split_extra_args(&extra);
        assert_eq!(cargo, &["--test", "cli"]);
        assert_eq!(libtest, &["--ignored", "--nocapture"]);
    }

    #[test]
    fn split_extra_args_empty_input() {
        let extra: Vec<String> = Vec::new();
        let (cargo, libtest) = split_extra_args(&extra);
        assert!(cargo.is_empty());
        assert!(libtest.is_empty());
    }

    #[test]
    fn has_target_selector_detects_explicit_targets() {
        // A profile's `--test <name>` scope, or any user-supplied target
        // flag, means doctests are already off and `--tests` must not be
        // appended on top.
        assert!(has_target_selector(&s(&["test", "--test", "cli_sort"])));
        assert!(has_target_selector(&s(&["test", "--tests"])));
        assert!(has_target_selector(&s(&["test", "--lib"])));
        assert!(has_target_selector(&s(&["test", "--bins"])));
        assert!(has_target_selector(&s(&["test", "--bin=foo"])));
        assert!(has_target_selector(&s(&["test", "--doc"])));
        assert!(has_target_selector(&s(&["test", "--all-targets"])));
        assert!(has_target_selector(&s(&["test", "--example", "e"])));
        assert!(has_target_selector(&s(&["test", "--bench", "b"])));
    }

    #[test]
    fn has_target_selector_ignores_non_target_flags() {
        // The default sweep shape: feature scoping + package, no target
        // selector - so `--tests` is what suppresses doctests here.
        assert!(!has_target_selector(&s(&[
            "test",
            "-p",
            "pkg",
            "--features",
            "a,b",
            "--message-format=json",
        ])));
        assert!(!has_target_selector(&s(&[
            "test",
            "--workspace",
            "--exclude",
            "slow",
        ])));
    }

    #[test]
    fn build_test_env_emits_absolute_paths() {
        // B8: CARGO_TARGET_TMPDIR used to be the literal string
        // "target/tmp", which only resolves correctly when the cargo
        // subprocess inherits cwd=project_root. Make sure the helper
        // emits an absolute path joined onto the cargo-resolved
        // target_dir.
        let target = Path::new("/home/u/proj/target");
        let env = build_test_env(Some(Project::Nidhogg), target, "debug");
        let tmp = env
            .iter()
            .find(|(k, _)| k == "CARGO_TARGET_TMPDIR")
            .map(|(_, v)| v.as_str())
            .expect("CARGO_TARGET_TMPDIR set for nidhogg");
        assert_eq!(tmp, "/home/u/proj/target/tmp");
        let bin = env
            .iter()
            .find(|(k, _)| k == "BROKKR_TEST_BIN_DIR")
            .map(|(_, v)| v.as_str())
            .expect("BROKKR_TEST_BIN_DIR set");
        assert_eq!(bin, "/home/u/proj/target/debug");
    }

    #[test]
    fn build_test_env_no_tmpdir_for_non_nidhogg() {
        let env = build_test_env(Some(Project::Pbfhogg), Path::new("/x/target"), "release");
        assert!(env.iter().all(|(k, _)| k != "CARGO_TARGET_TMPDIR"));
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "BROKKR_TEST_BIN_DIR")
                .map(|(_, v)| v.as_str()),
            Some("/x/target/release"),
        );
    }

    fn rustflags_sweep() -> ResolvedSweep {
        ResolvedSweep {
            rustflags: vec!["--cfg".into(), "madsim".into()],
            ..Default::default()
        }
    }

    fn get<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn rustflags_value(env: &[(String, String)]) -> Option<&str> {
        get(env, "RUSTFLAGS").or_else(|| get(env, "CARGO_ENCODED_RUSTFLAGS"))
    }

    #[test]
    fn sweep_runtime_env_isolates_target_dir_for_rustflags() {
        let sweep = rustflags_sweep();
        let key = crate::config::rustflags_target_key(&sweep.rustflags).unwrap();
        let env = sweep_runtime_env(
            &sweep,
            Some(Project::Pbfhogg),
            Path::new("/meta/target"),
            "debug",
        );
        // S3-20: the isolated dir sits beside cargo's resolved target dir, not
        // under an assumed `<project_root>/target`.
        let dir = format!("/meta/target/rustflags-{key}");
        assert_eq!(get(&env, "CARGO_TARGET_DIR"), Some(dir.as_str()));
        // BROKKR_TEST_BIN_DIR tracks the isolated dir, not the plain one.
        assert_eq!(get(&env, "BROKKR_TEST_BIN_DIR"), Some(format!("{dir}/debug").as_str()));
        // RUSTFLAGS carries the sweep's flags (composed with any inherited).
        let rf = rustflags_value(&env).expect("rustflags env set");
        assert!(rf.contains("--cfg") && rf.contains("madsim"), "got: {rf}");
    }

    #[test]
    fn sweep_runtime_env_isolated_dir_follows_offroot_target() {
        // S3-20 regression: a workspace `.cargo/config.toml` can put the target
        // dir on another drive. The isolated dir must land there, not on the
        // project root's drive (where `brokkr clean` never reclaims it).
        let sweep = rustflags_sweep();
        let key = crate::config::rustflags_target_key(&sweep.rustflags).unwrap();
        let env = sweep_runtime_env(
            &sweep,
            Some(Project::Pbfhogg),
            Path::new("/media/folk/Banan/cargo"),
            "debug",
        );
        assert_eq!(
            get(&env, "CARGO_TARGET_DIR"),
            Some(format!("/media/folk/Banan/cargo/rustflags-{key}").as_str())
        );
    }

    #[test]
    fn sweep_runtime_env_plain_sweep_uses_metadata_target() {
        let env = sweep_runtime_env(
            &ResolvedSweep::default(),
            Some(Project::Pbfhogg),
            Path::new("/meta/target"),
            "debug",
        );
        assert_eq!(get(&env, "BROKKR_TEST_BIN_DIR"), Some("/meta/target/debug"));
        assert!(get(&env, "CARGO_TARGET_DIR").is_none());
        assert!(rustflags_value(&env).is_none());
    }

    #[test]
    fn sweep_cargo_env_omits_bin_dir() {
        let key = crate::config::rustflags_target_key(&rustflags_sweep().rustflags).unwrap();
        let env = sweep_cargo_env(&rustflags_sweep(), Path::new("/meta/target"));
        // Clippy shares the same isolated dir the test phase builds into.
        assert_eq!(
            get(&env, "CARGO_TARGET_DIR"),
            Some(format!("/meta/target/rustflags-{key}").as_str())
        );
        assert!(rustflags_value(&env).is_some());
        // Clippy has no test binary to spawn.
        assert!(get(&env, "BROKKR_TEST_BIN_DIR").is_none());
        // A plain sweep contributes nothing.
        assert!(sweep_cargo_env(&ResolvedSweep::default(), Path::new("/meta/target")).is_empty());
    }

    fn parsed(passed: usize, failed: usize, ignored: usize, filtered_out: usize, suites: usize) -> cargo_filter::ParsedTestResults {
        cargo_filter::ParsedTestResults {
            failures: Vec::new(),
            passed,
            failed,
            ignored,
            filtered_out,
            suites,
            duration: None,
        }
    }

    #[test]
    fn zero_test_run_flags_zero_suites() {
        // Cargo exited 0 but the parser never saw a `test result:` line.
        // Either parse failure or `--test cli_x` matched no test crate.
        assert!(zero_test_run(&parsed(0, 0, 0, 0, 0)));
    }

    #[test]
    fn zero_test_run_flags_all_filtered_out() {
        // The classic silent-wrong-run shape: `--skip` or the positional
        // name filter excluded every test in the matched suite(s).
        assert!(zero_test_run(&parsed(0, 0, 0, 5, 1)));
    }

    #[test]
    fn zero_test_run_does_not_flag_empty_suite() {
        // A package that legitimately defines no tests reports
        // `0 passed; 0 filtered out` over one suite. Not a wrong run.
        assert!(!zero_test_run(&parsed(0, 0, 0, 0, 1)));
    }

    #[test]
    fn zero_test_run_does_not_flag_normal_pass() {
        assert!(!zero_test_run(&parsed(12, 0, 0, 3, 1)));
    }

    #[test]
    fn zero_test_run_does_not_flag_only_ignored() {
        // `#[ignore]` tests still count - the user opted in.
        assert!(!zero_test_run(&parsed(0, 0, 4, 0, 1)));
    }

    #[test]
    fn split_extra_args_only_dashes_separator() {
        // Just `-- --` after the brokkr `--`. First `--` is consumed by
        // clap, the second is our split point: both sides empty.
        let extra = s(&["--"]);
        let (cargo, libtest) = split_extra_args(&extra);
        assert!(cargo.is_empty());
        assert!(libtest.is_empty());
    }

    fn sweep(label: &str) -> ResolvedSweep {
        ResolvedSweep {
            label: label.into(),
            ..Default::default()
        }
    }

    #[test]
    fn describe_sweep_reports_package_scope() {
        // Whole workspace.
        assert_eq!(describe_sweep(&sweep("default"), false, None), "workspace");

        // `-p` scoped.
        let scoped = ResolvedSweep {
            packages: s(&["nautilus-core", "nautilus-model"]),
            ..sweep("ffi")
        };
        assert_eq!(describe_sweep(&scoped, false, None), "2 pkgs");

        // `--workspace --exclude` is a test-phase-only shape; the clippy line
        // stays workspace-wide, matching what actually runs.
        let excluded = ResolvedSweep {
            test_exclude_packages: s(&["nautilus-pyo3", "nautilus-cli"]),
            ..sweep("default")
        };
        assert_eq!(describe_sweep(&excluded, false, None), "workspace");
        assert_eq!(
            describe_sweep(&excluded, true, None),
            "workspace -2 pkgs, serial"
        );
    }

    #[test]
    fn describe_sweep_reads_features_back_out_of_argv() {
        let all = ResolvedSweep {
            cargo_feature_args: s(&["--all-features"]),
            ..sweep("all")
        };
        assert_eq!(describe_sweep(&all, false, None), "workspace, all-features");

        let consumer = ResolvedSweep {
            cargo_feature_args: s(&["--no-default-features", "--features", "commands"]),
            ..sweep("consumer")
        };
        assert_eq!(
            describe_sweep(&consumer, false, None),
            "workspace, no-default +commands"
        );

        // The `--features=x,y` spelling is equivalent.
        let joined = ResolvedSweep {
            cargo_feature_args: s(&["--features=ffi,live"]),
            ..sweep("j")
        };
        assert_eq!(describe_sweep(&joined, false, None), "workspace, +ffi,live");
    }

    #[test]
    fn describe_sweep_does_not_restate_the_label() {
        // The legacy no-`[[check]]` path names its synthesized sweep after the
        // feature shape, which would otherwise print twice on one line.
        let legacy = ResolvedSweep {
            cargo_feature_args: s(&["--all-features"]),
            ..sweep("all-features")
        };
        assert_eq!(describe_sweep(&legacy, false, None), "workspace");
        assert_eq!(
            sweep_run_line("clippy", &legacy, &[], false, false, None),
            "clippy all-features: workspace"
        );
    }

    #[test]
    fn describe_sweep_surfaces_rustflags_and_isolation() {
        // rustflags silently redirect the sweep to its own target dir; the
        // collapsed form must not hide the cause of a full recompile.
        let sim = ResolvedSweep {
            rustflags: s(&["--cfg", "madsim"]),
            ..sweep("sim")
        };
        assert!(describe_sweep(&sim, false, None).contains("rustflags --cfg madsim"));
        assert!(describe_sweep(&sim, false, None).contains("isolated target"));
    }

    #[test]
    fn describe_sweep_summarises_libtest_filters_by_count() {
        // The 14-skip list is the bulk of nautilus's command line and is
        // identical across its three sweeps - a count is the whole signal.
        let mut libtest_args = Vec::new();
        for name in ["a", "b", "c"] {
            libtest_args.push("--skip".to_owned());
            libtest_args.push(name.to_owned());
        }
        libtest_args.push("--include-ignored".to_owned());
        let tier = ResolvedSweep {
            libtest_args,
            test_threads: Some(0),
            ..sweep("tier1")
        };
        assert_eq!(
            describe_sweep(&tier, true, None),
            "workspace, 3 skips, include-ignored, parallel"
        );
        // Clippy never takes libtest filters, so its line omits them.
        assert_eq!(describe_sweep(&tier, false, None), "workspace");
    }

    #[test]
    fn describe_sweep_joins_test_filter_pairs() {
        // S3-35: `cargo_test_filters` is flattened `["--test", "cli_sort"]`;
        // the shape must render each filter as one item, not `--test` and the
        // bare name as two comma-separated fragments.
        let one = ResolvedSweep {
            cargo_test_filters: s(&["--test", "cli_sort"]),
            ..sweep("sort")
        };
        assert_eq!(
            describe_sweep(&one, true, None),
            "workspace, --test cli_sort, serial"
        );

        // Two filters stay two distinct items, each self-contained.
        let two = ResolvedSweep {
            cargo_test_filters: s(&["--test", "cli_sort", "--test", "cli_env"]),
            ..sweep("sort")
        };
        assert_eq!(
            describe_sweep(&two, true, None),
            "workspace, --test cli_sort, --test cli_env, serial"
        );

        // Clippy never takes cargo test filters, so its line omits them.
        assert_eq!(describe_sweep(&one, false, None), "workspace");
    }

    #[test]
    fn describe_sweep_thread_policy_tracks_watchdog_lane() {
        // None and Some(1) both mean the serial per-test watchdog lane.
        for threads in [None, Some(1)] {
            let serial = ResolvedSweep {
                test_threads: threads,
                ..sweep("serial")
            };
            assert_eq!(describe_sweep(&serial, true, None), "workspace, serial");
        }
        for threads in [Some(0), Some(4)] {
            let parallel = ResolvedSweep {
                test_threads: threads,
                ..sweep("par")
            };
            assert_eq!(describe_sweep(&parallel, true, None), "workspace, parallel");
        }
    }

    #[test]
    fn cli_package_replaces_sweep_selection_or_skips() {
        // Workspace sweep: `-p` applies; without `-p` the sweep stands.
        assert_eq!(
            cli_package_scope(&sweep("default"), Some("x"), true).unwrap(),
            Some("x")
        );
        assert_eq!(cli_package_scope(&sweep("default"), None, true).unwrap(), None);

        // The exclusion list rules the package out - but only for the test
        // phase; clippy ignores test_exclude_packages by design.
        let excluded = ResolvedSweep {
            test_exclude_packages: s(&["x"]),
            ..sweep("default")
        };
        assert!(cli_package_scope(&excluded, Some("x"), true).is_err());
        assert_eq!(
            cli_package_scope(&excluded, Some("x"), false).unwrap(),
            Some("x")
        );

        // A `packages` list admits members and skips everything else.
        let scoped = ResolvedSweep {
            packages: s(&["a", "b"]),
            ..sweep("ffi")
        };
        assert_eq!(cli_package_scope(&scoped, Some("a"), true).unwrap(), Some("a"));
        assert!(cli_package_scope(&scoped, Some("x"), false).is_err());
    }

    #[test]
    fn describe_sweep_reports_cli_package_scope() {
        // The nautilus bug shape: an exclude-carrying sweep under CLI `-p`
        // must say `-p x`, not `workspace -2 pkgs` - the shape describes
        // what runs, and the CLI scope replaced the sweep's selection.
        let excluded = ResolvedSweep {
            test_exclude_packages: s(&["a", "b"]),
            ..sweep("default")
        };
        assert_eq!(describe_sweep(&excluded, true, Some("x")), "-p x, serial");
        assert_eq!(describe_sweep(&excluded, false, Some("x")), "-p x");
    }

    #[test]
    fn sweep_run_line_switches_on_commands_flag() {
        let ffi = ResolvedSweep {
            packages: s(&["nautilus-core"]),
            cargo_feature_args: s(&["--features", "ffi"]),
            ..sweep("ffi")
        };
        let args = s(&["clippy", "-p", "nautilus-core", "--features", "ffi"]);

        assert_eq!(
            sweep_run_line("clippy", &ffi, &args, false, false, None),
            "clippy ffi: 1 pkgs, +ffi"
        );
        assert_eq!(
            sweep_run_line("clippy", &ffi, &args, true, true, None),
            "cargo clippy -p nautilus-core --features ffi"
        );
    }
}
