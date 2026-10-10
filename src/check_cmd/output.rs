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
/// A package-mode sweep isolates too, even with no `rustflags`: it resolves a
/// different feature graph for the same source, so sharing the common dir would
/// have it rebuild against every ordinary sweep on every run, in both
/// directions. Its dir is shared across the sweep's per-package resolutions -
/// the resolution boundary is not the artifact boundary, and a lane exports one
/// `BROKKR_TEST_BIN_DIR`, so a directory per package would leave a multi-package
/// lane with no single directory holding the binaries its tests spawn.
///
/// The name for the rustflags-only case is unchanged (`rustflags-<hash>`), so no
/// existing tree re-derives a directory or pays a cold rebuild for this feature.
pub(crate) fn isolated_target_dir(
    sweep: &ResolvedSweep,
    meta_target_dir: &Path,
) -> Option<std::path::PathBuf> {
    crate::config::shape_target_key(&sweep.rustflags, sweep.effective_unification)
        .map(|key| meta_target_dir.join(if key.starts_with("unify-") {
            key
        } else {
            format!("rustflags-{key}")
        }))
}

/// Compose a sweep's `rustflags` with any inherited flags into the env pair to
/// export. Appends to an inherited `CARGO_ENCODED_RUSTFLAGS` (0x1f-separated)
/// when present - cargo ignores `RUSTFLAGS` once the encoded form is set - else
/// to `RUSTFLAGS` (space-joined, matching `make cargo-test-sim`). `None` for an
/// empty flag list.
/// `extra` carries the `[lints] allow` flags when - and only when - the
/// environment is the layer cargo will actually read (see [`crate::rustflags`]).
/// They go last, because rustc resolves conflicting lint levels last-wins and
/// the whole point is to beat the project's `-Dwarnings`. When the config layer
/// is the live one, `extra` is empty here and the flags ride cargo's
/// `--config` instead.
fn composed_rustflags_env(rustflags: &[String], extra: &[String]) -> Option<(String, String)> {
    if rustflags.is_empty() && extra.is_empty() {
        return None;
    }
    if let Ok(existing) = std::env::var("CARGO_ENCODED_RUSTFLAGS") {
        let mut parts: Vec<String> = if existing.is_empty() {
            Vec::new()
        } else {
            existing.split('\u{1f}').map(str::to_owned).collect()
        };
        parts.extend(rustflags.iter().cloned());
        parts.extend(extra.iter().cloned());
        return Some(("CARGO_ENCODED_RUSTFLAGS".into(), parts.join("\u{1f}")));
    }
    let mut flags = std::env::var("RUSTFLAGS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_default();
    for flag in rustflags.iter().chain(extra) {
        if !flags.is_empty() {
            flags.push(' ');
        }
        flags.push_str(flag);
    }
    Some(("RUSTFLAGS".into(), flags))
}


/// The `CARGO_TARGET_DIR` + `RUSTFLAGS` pair a sweep's `rustflags` imply, or an
/// empty vec when it sets none. Used by the clippy phase (which needs no
/// `BROKKR_TEST_BIN_DIR`); the test phase gets the same knobs plus the bin dir
/// through [`sweep_runtime_env`].
/// `allow_flags` carries the environment's share of the `[lints] allow` flags,
/// on the same terms as [`sweep_runtime_env`] - empty when they ride cargo's
/// `--config`, and empty too for a caller that passes `-A` on its own argv
/// (clippy does). A caller that COMPILES and does neither gets a build without
/// the suppressions, which is a hard error wherever the project's `-Dwarnings`
/// turns one of those lints into one.
pub(crate) fn sweep_cargo_env(
    sweep: &ResolvedSweep,
    meta_target_dir: &Path,
    allow_flags: &[String],
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(dir) = isolated_target_dir(sweep, meta_target_dir) {
        out.push(("CARGO_TARGET_DIR".into(), dir.to_string_lossy().into_owned()));
    }
    out.extend(composed_rustflags_env(&sweep.rustflags, allow_flags));
    out
}

/// The full base env for one sweep's test/pre-build cargo runs: `build_test_env`
/// computed against the sweep's *effective* target dir (its isolated
/// `target/rustflags-<hash>` when it carries `rustflags`, else cargo's own
/// `meta_target_dir`), plus the `CARGO_TARGET_DIR` / `RUSTFLAGS` overlay. The
/// sweep's own `env` still overlays this via `merged_env`.
/// `allow_flags` is whatever share of the `[lints] allow` flags belongs in the
/// environment ([`crate::rustflags::plumbing`]); empty when they ride cargo's
/// `--config` instead. The caller decides, because the same sink answer also
/// selects the cargo args it must pass alongside these.
pub(crate) fn sweep_runtime_env(
    sweep: &ResolvedSweep,
    project: Option<Project>,
    meta_target_dir: &Path,
    profile_dir: &str,
    allow_flags: &[String],
) -> Vec<(String, String)> {
    let isolated = isolated_target_dir(sweep, meta_target_dir);
    let effective: &Path = isolated.as_deref().unwrap_or(meta_target_dir);

    let mut out = build_test_env(project, effective, profile_dir);
    if let Some(dir) = &isolated {
        out.push(("CARGO_TARGET_DIR".into(), dir.to_string_lossy().into_owned()));
    }
    out.extend(composed_rustflags_env(&sweep.rustflags, allow_flags));
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
/// a claim about the workspace.
pub(crate) fn describe_sweep(
    sweep: &ResolvedSweep,
    for_test: bool,
    selection: &Selection,
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
        describe_features(&sweep.cargo_feature_args).filter(|feat| *feat != sweep.label),
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
    if let Some(mode) = sweep.effective_unification.describe() {
        let isolated = if sweep.effective_unification.is_per_package() {
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
            if sweep.doc_only {
                // Serial by construction, and the lane's whole identity: an
                // unexplained `--doc` invocation is the one thing a collapsed
                // log must not hide.
                "doc-only"
            } else if matches!(sweep.test_threads, Some(n) if n != 1) {
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

/// The `cargo build` argv for one `build_packages` pre-build.
///
/// The pre-build is one of the sweep's compiling paths, so it carries the
/// sweep's pinned unification like every other: the binaries the tests spawn
/// must come from the same feature graph as the tests themselves, or a
/// package-mode lane runs its tests against a binary built under ambient
/// resolution.
fn pre_build_args(sweep: &ResolvedSweep, package: &str, allow_args: &[String]) -> Vec<String> {
    // `json-render-diagnostics`: the artifact stream on stdout (the support
    // executables the pre-build produced, which a complete plan hashes),
    // compiler diagnostics still rendered as text on stderr for the failure
    // path. The message format does not enter cargo's fingerprint.
    let mut args: Vec<String> = vec!["build".into(), "--message-format=json-render-diagnostics".into()];
    args.extend(allow_args.iter().cloned());
    // The pre-build must land where the tests will look for it: a sweep
    // pinned to a profile builds its binaries into that profile's
    // directory, which is the one BROKKR_TEST_BIN_DIR names.
    args.extend(sweep_profile_args(sweep));
    args.extend(sweep.unification_args());
    for f in &sweep.cargo_feature_args {
        args.push(f.clone());
    }
    // Its own explicit single-package selection, never the lane's test
    // selection: a support package is often outside what the lane tests.
    args.extend(package_args(&Selection::Explicit(vec![package.to_owned()])));
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
/// executables it produced. Errors surface compile failures the same way the
/// test phase does: the stderr filtered through `cargo_filter::filter_clippy`.
fn run_sweep_pre_build(
    project_root: &Path,
    sweep: &ResolvedSweep,
    package: &str,
    project_env: &[(String, String)],
    allow_args: &[String],
    commands: bool,
) -> Result<Vec<SupportArtifact>, DevError> {
    let args = pre_build_args(sweep, package, allow_args);

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
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, &env_refs)?;

    if captured.status.success() {
        return Ok(support_artifacts(&String::from_utf8_lossy(&captured.stdout), package));
    }

    let stderr = String::from_utf8_lossy(&captured.stderr);
    // Printed even under `--commands`: the streamed line is neither adjacent
    // to this failure nor attributable among several runs.
    // The identity leads and the error is `Reported`: nothing downstream
    // restates it, so this line is the only place the package and sweep appear
    // in the failure output.
    let failed = format!("build failed for package '{package}' in sweep '{}'", sweep.label);
    output::error(&failed);
    output::error(&format!("failing command: cargo {}", args.join(" ")));
    output::error(&cargo_filter::filter_clippy(&stderr));
    Err(DevError::Reported(failed))
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

/// Why a serial sweep cannot isolate its harnesses, or `None` when it can.
///
/// The isolation installs brokkr as cargo's host target runner and rustdoc (see
/// `test_runner::harness_shim`), which is only safe when cargo would execute host
/// binaries directly: an override would silently drop a configured runner or
/// rustdoc, and a non-host target never reaches a host runner at all. Anything
/// ambiguous takes the first-failure path rather than guessing.
fn serial_shim_fallback(
    root: &Path,
    forwarded: &[String],
    project_env: &[(String, String)],
    sweep_env: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let env_has = |key: &str| {
        project_env.iter().any(|(name, _)| name == key) || sweep_env.contains_key(key)
    };
    if forwarded.iter().any(|arg| {
        arg == "--config" || arg.starts_with("--config=") || arg == "--target" || arg.starts_with("--target=")
    }) {
        return Some("forwarded --config or --target".into());
    }
    if std::env::var_os("CARGO_BUILD_TARGET").is_some() || env_has("CARGO_BUILD_TARGET") {
        return Some("CARGO_BUILD_TARGET is set".into());
    }
    if std::env::var_os("RUSTDOC").is_some()
        || std::env::var_os("CARGO_BUILD_RUSTDOC").is_some()
        || env_has("RUSTDOC")
        || env_has("CARGO_BUILD_RUSTDOC")
    {
        return Some("rustdoc executable is configured".into());
    }
    // The runner execs brokkr through `/proc/<pid>/exe` (see
    // `harness_shim::own_exe`), so only a missing `/proc` rules it out.
    if std::fs::metadata("/proc/self/exe").is_err() {
        return Some("/proc/self/exe is unreadable, so cargo cannot exec brokkr as the runner".into());
    }
    let Some(host) = crate::rustflags::host_triple() else {
        return Some("host target could not be determined".into());
    };
    let runner_var = format!("CARGO_TARGET_{}_RUNNER", host.to_uppercase().replace('-', "_"));
    if std::env::var_os(&runner_var).is_some() || env_has(&runner_var) {
        return Some(format!("{runner_var} is set"));
    }
    for path in crate::rustflags::config_paths(root) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = text.parse::<toml::Table>() else {
            continue;
        };
        if let Some(build) = doc.get("build").and_then(toml::Value::as_table) {
            if build.contains_key("target") {
                return Some(format!("{} sets build.target", path.display()));
            }
            if build.contains_key("rustdoc") {
                return Some(format!("{} sets build.rustdoc", path.display()));
            }
        }
        if let Some(targets) = doc.get("target").and_then(toml::Value::as_table) {
            for (selector, value) in targets {
                if !value.as_table().is_some_and(|table| table.contains_key("runner")) {
                    continue;
                }
                // An undecidable cfg counts as applying: overriding a real
                // runner is the destructive direction.
                let applies = selector
                    .strip_prefix("cfg(")
                    .and_then(|s| s.strip_suffix(')'))
                    .map_or(selector == &host, |expr| crate::rustflags::eval_cfg(expr) != Some(false));
                if applies {
                    return Some(format!("{} configures a target runner", path.display()));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod serial_shim_tests {
    use super::serial_shim_fallback;

    #[test]
    fn forwarded_target_and_config_force_first_failure_mode() {
        let root = std::path::Path::new("/nonexistent");
        for arg in ["--target", "--target=other", "--config", "--config=target.x.runner=tool"] {
            let reason = serial_shim_fallback(root, &[arg.into()], &[], &Default::default())
                .expect("forwarded override must disable shim");
            assert!(reason.contains("forwarded"));
        }
    }

    #[test]
    fn explicit_target_env_forces_first_failure_mode() {
        let reason = serial_shim_fallback(
            std::path::Path::new("/nonexistent"),
            &[],
            &[("CARGO_BUILD_TARGET".into(), "other".into())],
            &Default::default(),
        )
        .expect("target env must disable shim");
        assert!(reason.contains("CARGO_BUILD_TARGET"));
    }
}

/// The cargo profile-selection fragment for one sweep (`--release`, or
/// nothing). Empty for a sweep that names no `profile`, which is every
/// sweep in a repo that never sets the key: `brokkr check` compiles dev,
/// exactly as it always has.
///
/// Emitted by every cargo run that COMPILES this sweep - clippy, the
/// pre-build, the test run, the process-isolated per-test invocations, and
/// the coverage enumeration - because a profile mismatch between any two of
/// them is a silent full rebuild at best and lint results for a build that
/// never ran at worst.
fn sweep_profile_args(sweep: &ResolvedSweep) -> Vec<String> {
    sweep
        .profile
        .map(|p| p.cargo_args().iter().map(|a| (*a).to_owned()).collect())
        .unwrap_or_default()
}

/// The cargo-level selection + feature args shared by the standard and
/// process-isolated test paths, so the two can never diverge on what a
/// sweep selects. The package half is the invocation selection's
/// ([`package_args`]): a CLI `-p` set already *replaced* the sweep's own
/// selection there - cargo unions selection flags, so emitting `--workspace
/// --exclude ... -p X` would silently run the whole workspace.
fn sweep_selection_args(sweep: &ResolvedSweep, selection: &Selection) -> Vec<String> {
    let mut args: Vec<String> = sweep_profile_args(sweep);
    // The sweep's pinned resolution. This is the ordinary (non-parallel) test
    // path, and it was the one place the pin was never emitted: the lane was
    // labelled `unification package`, isolated into its own target dir, and
    // then compiled under whatever the config chain said - a shape asserted in
    // config that was not the shape that ran, which is precisely the failure
    // this feature exists to catch, reproduced inside the feature itself.
    args.extend(sweep.unification_args());
    // `-p <pkg>` scoping is also what makes `--features` valid in a virtual
    // workspace.
    args.extend(package_args(selection));
    for f in &sweep.cargo_feature_args {
        args.push(f.clone());
    }
    for f in &sweep.cargo_test_filters {
        args.push(f.clone());
    }
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
                    event: crate::test_runner::ObsEvent::AttributionError {
                        detail: format!(
                            "cargo exited successfully but the planned harness {} ({exe}) never \
                             presented its stream",
                            unit.id()
                        ),
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
    sweep: &ResolvedSweep,
    run: &ResolutionRun,
    extra_args: &[String],
    project_env: &[(String, String)],
    allow_args: &[String],
    doctests: bool,
    multi: bool,
    commands: bool,
    timings: Option<&mut Vec<TestTiming>>,
    observe: &SerialObserve<'_>,
) -> Result<bool, DevError> {
    let (cargo_extra, libtest_extra) = split_extra_args(extra_args);
    // A parallel sweep (test_threads != 1) takes the parallel runner below and
    // never isolates; a serial one isolates its harnesses unless it cannot.
    // (Under a complete profile neither escape exists: preparation refused
    // both, since neither can be attributed.)
    let parallel_threads = matches!(sweep.test_threads, Some(n) if n != 1);
    let shim_reason = if parallel_threads {
        None
    } else {
        serial_shim_fallback(project_root, cargo_extra, project_env, &sweep.env)
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

    let mut args: Vec<String> = vec!["test".into()];
    // Before the selection and the `--` split: `--config` is a cargo option,
    // and everything past `--` belongs to libtest.
    args.extend(allow_args.iter().cloned());
    args.extend(sweep_selection_args(sweep, &run.selection));
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
    if sweep.doc_only {
        args.push("--doc".into());
    } else if !doctests && !has_target_selector(&args) {
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
    } else if test_runner::effective_test_threads(&libtest_args)?.is_none() {
        libtest_args.push("--test-threads=1".into());
    }
    // Both lanes drive libtest's JSON event stream. The parallel lane always
    // needed it - human output emits no per-test *start* signal once tests run
    // concurrently - and the serial lane now uses it too, so the per-test budget
    // is charged from records libtest states rather than from a partial
    // `test NAME ... ` marker reconstructed out of whatever the test printed
    // alongside it. Native on nightly.
    libtest_args.push("-Z".into());
    libtest_args.push("unstable-options".into());
    libtest_args.push("--format".into());
    libtest_args.push("json".into());
    for e in libtest_extra {
        libtest_args.push(e.clone());
    }
    // Checked on the FINAL argv, after the forwarded `-- …` args are appended,
    // because libtest takes the last occurrence of a repeated flag. Checking only
    // the profile's own args let `brokkr check -- --format pretty` (or
    // `--format=pretty`) override the injected JSON: the drain would then read
    // human output, never observe a lifecycle event, and the promised 20s per-test
    // cap would silently degrade to the five-minute idle ceiling. Both spellings,
    // since `--format=pretty` is one argv entry.
    reject_format_override(&libtest_args)?;
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

    let command = if multi {
        format!("cargo {} (sweep: {})", args.join(" "), sweep.label)
    } else {
        format!("cargo {}", args.join(" "))
    };
    announce_sweep(
        &format!("test {}: {}", sweep.label, describe_sweep(sweep, true, &run.selection)),
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

    // Serial => the watchdog runner; parallel => the whole-sweep-timeout
    // runner. Both reduce to (captured, optional hung test, timed_out, per-test
    // timings) so the reporting below is shared.
    let (captured, hung, timed_out, completed) = if parallel {
        let run = test_runner::run_libtest_parallel(
            "cargo",
            &arg_refs,
            project_root,
            state_root,
            &env_refs,
            test_runner::PARALLEL_SWEEP_TIMEOUT,
            test_runner::TEST_TIMEOUT,
            // One cargo invocation, so there are no siblings to cancel.
            None,
            Some(&sink),
            |_| {},
            |_| {},
            {
                let unit = unit.clone();
                move |elapsed| built_line(&unit, elapsed, " (parallel)")
            },
        )?;
        let hung = match run.outcome {
            LibtestOutcome::HungTest(h) => Some(h),
            LibtestOutcome::Completed => None,
        };
        (run.captured, hung, run.timed_out, run.completed)
    } else {
        let run = test_runner::streaming_run_libtest(
            test_runner::Launch::Cargo,
            &arg_refs,
            project_root,
            state_root,
            &env_refs,
            // Many tests in one process: the per-test clock can only name a
            // suspect, so the wall clock is what actually bounds the sweep.
            test_runner::Ceilings::shared_harness(),
            &obs,
            |_| {},
            |_| {},
            {
                let unit = unit.clone();
                move |elapsed| built_line(&unit, elapsed, "")
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
    if timed_out {
        output::error(&format!(
            "sweep '{}' exceeded the parallel test timeout ({}s) and was killed",
            sweep.label,
            test_runner::PARALLEL_SWEEP_TIMEOUT.as_secs(),
        ));
        output::error(&full_command);
        // `Reported`: the diagnosis and the failing command are printed above.
        return Err(DevError::Reported(format!(
            "sweep '{}' exceeded its time budget - stopping",
            sweep.label
        )));
    }

    if let Some(hung) = hung {
        // The sweep leads: `check` does not echo the `Reported` label below,
        // and neither the hung-test block nor the cargo line names it.
        output::error(&format!(
            "a test exceeded its {}s budget in sweep '{}' - stopping",
            hung.ceiling.as_secs(),
            sweep.label
        ));
        output::error(&test_runner::format_hung_test(&hung, project_root));
        output::error(&full_command);
        return Err(DevError::Reported(format!(
            "a test exceeded its {}s budget in sweep '{}' - stopping",
            hung.ceiling.as_secs(),
            sweep.label
        )));
    }

    if !captured.status.success() {
        output::error(&full_command);
        output::error(&cargo_filter::filter_test(&stdout, &stderr));
        return Ok(false);
    }
    observe.missing_streams();

    // A green sweep prints no test output. To watch one test run, run it:
    // `brokkr test <NAME>` streams its stdout/stderr live. A gate is not a
    // log tail, and the build warnings below are the one thing here that a
    // passing run still needs to say.
    // Held until the phase ends and merged with the identical block every other
    // sweep produced: one cargo warning seen by four sweeps is one warning.
    let filtered = cargo_filter::filter_clippy_in_tree(&stderr, Some(project_root));
    if filtered != "cargo clippy: no issues" {
        let relabeled = filtered.replacen("cargo clippy:", "cargo test:", 1);
        note_warning(&relabeled, &sweep.label);
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
    if let cargo_filter::Completeness::Incomplete { reason } = &parsed.completeness {
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

/// Refuse a libtest argv that could override the injected `--format json`.
///
/// Checked on the FINAL argv, after the forwarded `-- …` args are appended,
/// because libtest honours the LAST occurrence of a repeated flag. Checking only
/// the profile's own args let `brokkr check -- --format pretty` win: the drain
/// would read human output, never observe a lifecycle event, and the promised 20s
/// per-test cap would silently degrade to the five-minute idle ceiling. A silent
/// downgrade of the cap is the one failure this whole design exists to prevent.
///
/// Both spellings, since `--format=pretty` is a single argv entry. brokkr's own
/// injected pair is the one permitted occurrence.
fn reject_format_override(libtest_args: &[String]) -> Result<(), DevError> {
    let occurrences: Vec<&String> = libtest_args
        .iter()
        .filter(|a| *a == "--format" || a.starts_with("--format="))
        .collect();
    let brokkrs_own = occurrences.iter().filter(|a| **a == "--format").count();
    if occurrences.len() <= 1 && brokkrs_own == occurrences.len() {
        return Ok(());
    }
    Err(DevError::Config(format!(
        "brokkr drives libtest's JSON output to charge the per-test budget, and `{}` would \
         override it - libtest honours the last `--format` it is given, so the per-test cap would \
         degrade to the idle ceiling without saying so. Remove the `--format` override from this \
         profile's libtest_args or from the forwarded `-- ...` args.",
        occurrences
            .iter()
            .find(|a| ***a != "--format")
            .map_or("--format", |a| a.as_str())
    )))
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
    // A truncated stream is not a zero-test run - it is a run that stopped
    // reporting, and its zeros mean "never said" rather than "nothing matched".
    // Calling it a wrong-run would blame the filter for a crash; the caller's
    // exit-status and timeout paths own that case.
    if !p.is_complete() {
        return false;
    }
    p.accounted() == 0 && p.filtered_out > 0
}

/// Combine the sweep's profile-defined env with the project's
/// always-set vars (e.g. nidhogg's `CARGO_TARGET_TMPDIR`). Sweep
/// values come first; project values append (so a sweep can shadow a
/// project default if it really needs to).
///
/// The one composition rule for every phase - test, pre-build, clippy,
/// rustdoc and the coverage enumeration - so no two of them can disagree
/// about which side of a collision cargo sees.
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
        // --features commands → single ad-hoc sweep, taking no `[[check]]`
        // entry. (It still inherits profile run shaping when a `[test]`
        // section exists - see the ad_hoc_inherits_* tests below.)
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

    /// The regression this whole split exists for: `check -p <pkg>
    /// --features x` used to issue a bare `cargo test` with no `--skip` at
    /// all, because the ad-hoc branch dropped the profile's run shaping
    /// along with its sweep selection. It then failed on tests the default
    /// profile had always excluded (nautilus: 8 `serial_tests::` + 2
    /// `logging::macros::`, deterministic), reporting a red that reads as a
    /// code failure.
    #[test]
    fn ad_hoc_inherits_default_profile_run_shaping() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]
skip = ["serial_tests::", "logging::macros::"]
test_threads = 0
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: vec!["ignored".into()],
            ..Default::default()
        }];
        let sweeps = decide_active_sweeps(
            &entries,
            Some(&test_cfg),
            None,
            &["defi".to_owned()],
            false,
        )
        .unwrap();
        assert_eq!(sweeps.len(), 1);
        // Sweep selection IS overridden: the ad-hoc feature set, and no
        // `[[check]]` entry (hence no build_packages).
        assert_eq!(sweeps[0].label, "default");
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--features", "defi"]);
        assert!(sweeps[0].build_packages.is_empty());
        // Run shaping is NOT: the profile's filters and thread policy ride along.
        assert_eq!(
            sweeps[0].libtest_args,
            vec!["--skip", "serial_tests::", "--skip", "logging::macros::"]
        );
        assert_eq!(sweeps[0].test_threads, Some(0));
    }

    /// The quiet half of the same defect: entry-level `env` is a
    /// build-affecting invariant (the `HIGH_PRECISION` class), and an ad-hoc
    /// run dropping it went green having compiled a different width than the
    /// gate. `brokkr clippy`'s ad-hoc path already unioned it; `check` now
    /// takes the same union from the same function, so the two commands can't
    /// resolve different widths from one config.
    #[test]
    fn ad_hoc_inherits_unioned_check_entry_env() {
        let entries = vec![
            CheckEntry {
                name: "default".into(),
                env: [("HIGH_PRECISION".to_owned(), "1".to_owned())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            CheckEntry {
                name: "cli".into(),
                env: [("HIGH_PRECISION".to_owned(), "1".to_owned())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        ];
        let sweeps =
            decide_active_sweeps(&entries, None, None, &["defi".to_owned()], false).unwrap();
        assert_eq!(
            sweeps[0].env.get("HIGH_PRECISION").map(String::as_str),
            Some("1")
        );
    }

    /// Disagreement is a hard error naming the key, never a coin flip - the
    /// property that makes the union safe to take implicitly.
    #[test]
    fn ad_hoc_env_conflict_across_entries_errors() {
        let entries = vec![
            CheckEntry {
                name: "high".into(),
                env: [("HIGH_PRECISION".to_owned(), "1".to_owned())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            CheckEntry {
                name: "std".into(),
                env: [("HIGH_PRECISION".to_owned(), "0".to_owned())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        ];
        let err = decide_active_sweeps(&entries, None, None, &["defi".to_owned()], false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("HIGH_PRECISION"), "got: {err}");
        // The remedy must name flags `check` actually has - not clippy's
        // `--env` / `--sweep`.
        assert!(err.contains("run a profile"), "got: {err}");
    }

    /// Entry env overlays profile env on a key collision, matching
    /// `build_resolved_sweep`'s precedence for non-ad-hoc sweeps.
    #[test]
    fn ad_hoc_entry_env_overlays_profile_env() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]
env = { HIGH_PRECISION = "0", ONLY_PROFILE = "p" }
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            env: [("HIGH_PRECISION".to_owned(), "1".to_owned())]
                .into_iter()
                .collect(),
            ..Default::default()
        }];
        let sweeps = decide_active_sweeps(
            &entries,
            Some(&test_cfg),
            None,
            &["defi".to_owned()],
            false,
        )
        .unwrap();
        assert_eq!(
            sweeps[0].env.get("HIGH_PRECISION").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            sweeps[0].env.get("ONLY_PROFILE").map(String::as_str),
            Some("p")
        );
    }

    /// `test_exclude_packages` stays out of the union: a selection
    /// workaround, not an invariant, so inheriting it would narrow what a
    /// scoped run tests while the rest of the run claims to be wider.
    #[test]
    fn ad_hoc_does_not_inherit_test_exclude_packages() {
        let entries = vec![CheckEntry {
            name: "default".into(),
            test_exclude_packages: vec!["nautilus-pyo3".into()],
            ..Default::default()
        }];
        let sweeps =
            decide_active_sweeps(&entries, None, None, &["defi".to_owned()], false).unwrap();
        assert!(sweeps[0].test_exclude_packages.is_empty());
    }

    /// A `lanes` profile shapes nothing of its own, so an ad-hoc run under
    /// one inherits no filters rather than silently borrowing one lane's.
    #[test]
    fn ad_hoc_under_lanes_profile_inherits_nothing() {
        let toml_text = r#"
default_profile = "gate"

[profiles.gate]
lanes = ["tier1"]

[profiles.tier1]
sweeps = ["all"]
skip = ["serial_tests::"]
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            ..Default::default()
        }];
        let sweeps = decide_active_sweeps(
            &entries,
            Some(&test_cfg),
            None,
            &["defi".to_owned()],
            false,
        )
        .unwrap();
        assert_eq!(sweeps.len(), 1);
        assert!(sweeps[0].libtest_args.is_empty());
    }

    /// `--profile` still selects which shaping an ad-hoc run inherits.
    #[test]
    fn ad_hoc_honours_explicit_profile_for_shaping() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]
skip = ["tier1_only::"]

[profiles.edit]
sweeps = ["all"]
skip = ["edit_only::"]
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            ..Default::default()
        }];
        let sweeps = decide_active_sweeps(
            &entries,
            Some(&test_cfg),
            Some("edit"),
            &["defi".to_owned()],
            false,
        )
        .unwrap();
        assert_eq!(sweeps[0].libtest_args, vec!["--skip", "edit_only::"]);
    }

    /// No `[test]` section at all: nothing to inherit, and the ad-hoc sweep
    /// stays exactly as it was.
    #[test]
    fn ad_hoc_without_test_config_is_unshaped() {
        let sweeps =
            decide_active_sweeps(&[], None, None, &["defi".to_owned()], false).unwrap();
        assert!(sweeps[0].libtest_args.is_empty());
        assert_eq!(sweeps[0].test_threads, None);
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

    fn run(label: &str, selected: Option<&[&str]>) -> SweepResult {
        SweepResult {
            label: label.into(),
            shape: String::new(),
            command: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            success: true,
            selected: selected.map(|s| s.iter().map(|p| (*p).to_owned()).collect()),
            manifest: Vec::new(),
        }
    }

    /// `r` with its manifest lints attributed, as the clippy phase does.
    fn attributed(mut r: SweepResult, info: &crate::build::ProjectInfo) -> SweepResult {
        r.manifest = attribute_manifest_lints(&r, info);
        r
    }

    /// A clippy run that failed in a build script, as cargo reports it: stdout
    /// is JSON records only, stderr carries a manifest lint, its tally, a
    /// build-script error, cargo's status line and a build script's own
    /// warnings. The report lists the manifest lint as a diagnostic, keeps the
    /// error block minus the build script's directives, drops every JSON
    /// record, and counts only the warnings it really withheld.
    #[test]
    fn failed_build_script_run_reports_manifest_lints_and_the_error() {
        let info = two_member_info();
        let mut r = run("default", None);
        r.success = false;
        r.stdout = [
            r#"{"reason":"compiler-artifact","package_id":"registry+https://github.com/rust-lang/crates.io-index#regex@1.13.1","filenames":["a"]}"#,
            r#"{"reason":"build-script-executed","package_id":"registry+https://github.com/rust-lang/crates.io-index#libm@0.2.15"}"#,
            r#"{"reason":"build-finished","success":false}"#,
        ]
        .join("\n");
        r.stderr = "\
warning: unused dependency `anyhow`
  --> core/Cargo.toml:25:1
   |
25 | anyhow = { workspace = true }
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = note: `cargo::unused_dependencies` is set to `warn` by default
help: consider removing the dependency on `anyhow`
warning: `core` (manifest) generated 1 warning
   Compiling fontconfig-sys v6.0.1
error: failed to run custom build command for `fontconfig-sys v6.0.1`

Caused by:
  process didn't exit successfully: `build_script_build` (exit status: 101)
  --- stdout
  cargo:rerun-if-env-changed=PKG_CONFIG
  cargo:rerun-if-env-changed=PKG_CONFIG_PATH
  cargo:warning=probing fontconfig

  --- stderr
  The system library `fontconfig` required by crate `fontconfig-sys` was not found.
warning: build failed, waiting for other jobs to finish...
warning: desktop: Exported component 'A' doesn't inherit Window
warning: desktop: Exported component 'B' doesn't inherit Window
"
        .to_owned();
        let r = attributed(r, &info);

        let root = crate::test_scratch::scratch("check-output", "failed_build_script_run");
        let out = format_clippy_multi("cargo clippy", &[r], Some(&info), &root, false, &|_| true);

        assert!(out.starts_with("cargo clippy: 1 error\n"), "{out}");
        assert!(
            out.contains("error[cargo::unused_dependencies] core/Cargo.toml:25:1 unused dependency `anyhow`"),
            "{out}"
        );
        assert!(out.contains("error: failed to run custom build command"), "{out}");
        assert!(out.contains("`fontconfig` required by crate"), "{out}");
        assert!(!out.contains("\"reason\""), "{out}");
        // The listed lint is not repeated from the stream: its excerpt's
        // column-zero line number (`25 | ...`) is part of the diagnostic, not
        // foreign text that would force the block to print.
        assert!(!out.contains("consider removing"), "{out}");
        // A build script's directives go; its messages stay.
        assert!(!out.contains("rerun-if-env-changed"), "{out}");
        assert!(out.contains("cargo:warning=probing fontconfig"), "{out}");
        assert!(out.contains("(2 build-script directives not shown)"), "{out}");
        // Withheld: the two build-script warnings. Not the listed manifest
        // lint, not the tally, not cargo's `build failed` status.
        assert!(out.contains("2 warnings not shown"), "{out}");
        assert!(!out.contains("Exported component"), "{out}");
    }

    /// Under `--keep-going` one run can carry a rustc error the list shows and
    /// a build-script panic it cannot: the panic prints too, and the listed
    /// error is not repeated. A run whose only stderr error is cargo's
    /// `could not compile` summary prints nothing beyond the list.
    #[test]
    fn a_build_script_panic_beside_a_listed_error_is_shown() {
        let error = r#"{"reason":"compiler-message","package_id":"id-core","message":{"level":"error","code":{"code":"E0425"},"message":"cannot find value `x`","spans":[{"file_name":"core/src/a.rs","line_start":1,"column_start":1,"line_end":1,"column_end":2,"is_primary":true}],"children":[],"rendered":"RENDERED-E0425"}}"#;
        let root = crate::test_scratch::scratch("check-output", "a_build_script_panic_beside");

        let mut r = run("default", None);
        r.success = false;
        r.stdout = error.to_owned();
        r.stderr = "\
error: failed to run custom build command for `x-sys v1.0.0`

Caused by:
  boom
error: could not compile `core` (lib) due to 1 previous error
"
        .to_owned();
        let out = format_clippy_multi("cargo clippy", &[r], None, &root, false, &|_| true);
        assert!(out.contains("error[E0425] core/src/a.rs:1:1 cannot find value `x`"), "{out}");
        assert!(out.contains("failed to run custom build command for `x-sys"), "{out}");
        assert!(!out.contains("RENDERED-E0425"), "{out}");

        let mut r = run("default", None);
        r.success = false;
        r.stdout = error.to_owned();
        r.stderr = "error: could not compile `core` (lib) due to 1 previous error\n".to_owned();
        let out = format_clippy_multi("cargo clippy", &[r], None, &root, false, &|_| true);
        assert!(!out.contains("could not compile"), "{out}");
    }

    #[test]
    fn cargo_status_lines_are_not_warnings() {
        for status in [
            "warning: `a` (lib) generated 3 warnings",
            "warning: `a` (lib) generated 1 warning",
            "warning: `a` (lib) generated 3 warnings (1 duplicate)",
            "warning: `a` (lib test) generated 2 warnings (run `cargo clippy --fix --lib -p a --tests` to apply 2 suggestions)",
            "warning: `a` (manifest) generated 2 warnings",
            "warning: build failed, waiting for other jobs to finish...",
        ] {
            assert!(is_cargo_status(status), "{status}");
        }
        for warning in [
            "warning: unused dependency `a`",
            "warning: desktop: the build (x) generated output",
        ] {
            assert!(!is_cargo_status(warning), "{warning}");
        }
    }

    /// An error an allow filtered from the list is still why cargo failed: the
    /// report carries its rendered text, not an empty stream.
    #[test]
    fn a_filtered_error_still_explains_the_failure() {
        let mut r = run("default", None);
        r.success = false;
        r.stdout = [
            r#"{"reason":"compiler-message","package_id":"p","message":{"level":"error","code":{"code":"E0425"},"message":"cannot find value `x`","spans":[],"children":[],"rendered":"error[E0425]: cannot find value `x` in this scope\n --> src/a.rs:1:1\n"}}"#,
            r#"{"reason":"compiler-artifact","package_id":"q","filenames":["a"]}"#,
        ]
        .join("\n");
        let root = crate::test_scratch::scratch("check-output", "a_filtered_error");
        let out = format_clippy_multi("cargo clippy", &[r], None, &root, false, &|_| false);
        assert!(out.contains("error[E0425]: cannot find value `x` in this scope"), "{out}");
        assert!(!out.contains("compiler-artifact"), "{out}");
    }

    /// Text a warning header swallowed only because no header followed it is
    /// printed, not withheld with the warning.
    #[test]
    fn a_warning_block_with_foreign_text_is_printed() {
        let mut r = run("default", None);
        r.success = false;
        r.stderr = "\
warning: relaying tool output
TOOL CRASHED: out of disk
error: could not compile `a`
warning: plain one-liner
"
        .to_owned();
        let root = crate::test_scratch::scratch("check-output", "a_warning_block_with_foreign_text");
        let out = format_clippy_multi("cargo clippy", &[r], None, &root, false, &|_| true);
        assert!(out.contains("TOOL CRASHED: out of disk"), "{out}");
        assert!(out.contains("error: could not compile"), "{out}");
        assert!(!out.contains("plain one-liner"), "{out}");
        assert!(out.contains("1 warning not shown"), "{out}");
    }

    /// A denied cargo lint an allow filtered is cargo's reason for failing: its
    /// error block prints. And a tally that swallowed foreign text prints
    /// whole, like any other warning block.
    #[test]
    fn a_filtered_manifest_error_and_a_swallowing_tally_both_print() {
        let mut r = run("default", None);
        r.success = false;
        r.stderr = "\
error: unused dependency `anyhow`
  --> a/Cargo.toml:25:1
   = note: `cargo::unused_dependencies` is set to `deny` in `[lints]`
warning: `a` (manifest) generated 1 warning
TOOL CRASHED: out of disk
warning: `b` (manifest) generated 1 warning
"
        .to_owned();
        let r = attributed(r, &two_member_info());
        let root = crate::test_scratch::scratch("check-output", "a_filtered_manifest_error");
        let out = format_clippy_multi("cargo clippy", &[r], None, &root, false, &|_| false);
        assert!(out.contains("error: unused dependency `anyhow`"), "{out}");
        assert!(out.contains("TOOL CRASHED: out of disk"), "{out}");
        // The self-contained tally is dropped, and never counted.
        assert!(!out.contains("`b` (manifest)"), "{out}");
        assert!(!out.contains("not shown"), "{out}");
    }

    /// A manifest lint is attributed by the `Cargo.toml` it names, resolved
    /// against the workspace root: a member's follows the run's selection,
    /// the root manifest's is every run's, and any other manifest - outside
    /// the tree or inside it but not a member - is a dependency's, whose
    /// warning does not fail the gate.
    #[test]
    fn manifest_lints_are_attributed_by_manifest() {
        let info = two_member_info();
        let lint = |file: &str| {
            format!("warning: unused dependency `x`\n  --> {file}:1:1\n   = note: `cargo::unused_dependencies` is set to `warn` by default\n")
        };
        let stderr = ["core/Cargo.toml", "./daemon/../core/Cargo.toml", "Cargo.toml", "../sibling/Cargo.toml", "vendor/x/Cargo.toml"]
            .map(lint)
            .concat();
        let owners = |selected: Option<&[&str]>| -> Vec<(String, Option<String>)> {
            let mut r = run("r", selected);
            r.stderr.clone_from(&stderr);
            attribute_manifest_lints(&r, &info)
                .into_iter()
                .map(|(_, e)| (e.file.unwrap_or_default(), e.package_id))
                .collect()
        };
        let id = |s: &str| Some(s.to_owned());
        assert_eq!(
            owners(None),
            [
                ("core/Cargo.toml".to_owned(), id("id-core")),
                ("./daemon/../core/Cargo.toml".to_owned(), id("id-core")),
                ("Cargo.toml".to_owned(), None),
                ("../sibling/Cargo.toml".to_owned(), id("manifest+/sibling/Cargo.toml")),
                ("vendor/x/Cargo.toml".to_owned(), id("manifest+/ws/vendor/x/Cargo.toml")),
            ]
        );
        // `-p daemon`: core's manifest is not this run's; the root's still is.
        let narrowed: Vec<String> = owners(Some(&["daemon"])).into_iter().map(|(f, _)| f).collect();
        assert_eq!(narrowed, ["Cargo.toml", "../sibling/Cargo.toml", "vendor/x/Cargo.toml"]);

        let members = Some(&info.workspace_members);
        let dependency = |file: &str| {
            let r = attributed(
                SweepResult {
                    stderr: lint(file),
                    ..run("r", None)
                },
                &info,
            );
            is_dependency_warning(&r.manifest[0].1, members)
        };
        assert!(!dependency("core/Cargo.toml"));
        assert!(!dependency("Cargo.toml"));
        assert!(dependency("../sibling/Cargo.toml"));
        assert!(dependency("vendor/x/Cargo.toml"));
    }

    /// Two members, both default: `core` and `daemon`.
    fn two_member_info() -> crate::build::ProjectInfo {
        crate::build::ProjectInfo {
            target_dir: std::path::PathBuf::from("target"),
            bare_selection_is_whole_workspace: true,
            workspace_members: [
                ("id-core".to_owned(), "core".to_owned()),
                ("id-daemon".to_owned(), "daemon".to_owned()),
            ]
            .into(),
            default_members: ["id-core".to_owned(), "id-daemon".to_owned()].into(),
            workspace_root: std::path::PathBuf::from("/ws"),
            member_manifests: [
                (std::path::PathBuf::from("/ws/core/Cargo.toml"), "id-core".to_owned()),
                (std::path::PathBuf::from("/ws/daemon/Cargo.toml"), "id-daemon".to_owned()),
            ]
            .into(),
        }
    }

    /// Shape of the reporting run: a workspace sweep plus a sweep of one
    /// package. A daemon diagnostic both report is the code's, not a shape's.
    #[test]
    fn no_tag_when_every_covering_sweep_reported_it() {
        let info = two_member_info();
        let results = [run("default", None), run("daemon-shape", Some(&["daemon"]))];
        let both = ["default".to_owned(), "daemon-shape".to_owned()];
        assert_eq!(coverage_tag(&both, Some("id-daemon"), &results, Some(&info)), None);
        // A core diagnostic from the workspace sweep alone: the daemon sweep
        // never selected core, so nothing covering it stayed silent.
        let default_only = ["default".to_owned()];
        assert_eq!(coverage_tag(&default_only, Some("id-core"), &results, Some(&info)), None);
    }

    /// The case the tag exists for: a shape-specific diagnostic, here one the
    /// package's own sweep found and the workspace sweep did not.
    #[test]
    fn tag_when_a_covering_sweep_did_not_report_it() {
        let info = two_member_info();
        let results = [run("default", None), run("daemon-shape", Some(&["daemon"]))];
        let shape_only = ["daemon-shape".to_owned()];
        assert_eq!(
            coverage_tag(&shape_only, Some("id-daemon"), &results, Some(&info)),
            Some("[daemon-shape]".into())
        );
    }

    /// Without a package id, every run counts as covering.
    #[test]
    fn unknown_package_falls_back_to_every_run() {
        let info = two_member_info();
        let results = [run("default", None), run("daemon-shape", Some(&["daemon"]))];
        let default_only = ["default".to_owned()];
        assert_eq!(
            coverage_tag(&default_only, None, &results, Some(&info)),
            Some("[default]".into())
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

        let parsed = parse_clippy_from_json(&input, false, &[]);
        assert!(!parsed.parse_failed);
        assert_eq!(parsed.diagnostics.len(), 2);
        for d in &parsed.diagnostics {
            assert_eq!(d.header, "error[clippy::collapsible_if]");
        }
    }

    #[test]
    fn json_to_clippy_uses_primary_label_for_detail() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"file_name":"src/foo.rs","line_start":20,"column_start":5,"line_end":20,"column_end":10,"is_primary":true,"label":"expected `i32`, found `&str`"}],"children":[],"rendered":"rendered"}}"#;
        let parsed = parse_clippy_from_json(input, false, &[]);
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
        let parsed = parse_clippy_from_json(input, false, &[]);
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
        // header degrades gracefully to a bare `error`.
        let input = json_compiler_message(
            "warning",
            None,
            "something happened",
            "src/foo.rs",
            10,
            5,
        );
        let parsed = parse_clippy_from_json(&input, false, &[]);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(parsed.diagnostics[0].header, "error");
    }

    /// Only a warning from outside the workspace is exempt. The same warning
    /// from a member, one with no package id, and a dependency's error all
    /// count - and with no member list nothing is a dependency.
    #[test]
    fn only_a_dependency_warning_is_not_an_error() {
        let event = |level: &str, package: Option<&str>| {
            let mut json = json_compiler_message(level, Some("unused_imports"), "unused", "src/a.rs", 1, 1);
            if let Some(id) = package {
                json = json.replacen("{\"reason\"", &format!("{{\"package_id\":\"{id}\",\"reason\""), 1);
            }
            cargo_json::parse_cargo_diagnostics(&json).remove(0)
        };
        let members: HashMap<String, String> = [("member#a@1.0.0".to_owned(), "a".to_owned())].into();
        let dep = "path+file:///elsewhere#b@1.0.0";

        assert!(is_dependency_warning(&event("warning", Some(dep)), Some(&members)));
        assert!(!is_dependency_warning(&event("warning", Some("member#a@1.0.0")), Some(&members)));
        assert!(!is_dependency_warning(&event("warning", None), Some(&members)));
        assert!(!is_dependency_warning(&event("error", Some(dep)), Some(&members)));
        assert!(!is_dependency_warning(&event("warning", Some(dep)), None));
    }

    #[test]
    fn a_capped_lint_is_reported_as_an_error() {
        // Under `--cap-lints=warn` a deny lint arrives at `warning` level; it is
        // an error all the same, in the flag and the header.
        let input = json_compiler_message(
            "warning",
            Some("clippy::manual_string_new"),
            "empty String is being created manually",
            "src/a.rs",
            3,
            5,
        );
        let parsed = parse_clippy_from_json(&input, false, &[]);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert!(parsed.diagnostics[0].is_error);
        assert_eq!(
            parsed.diagnostics[0].header,
            "error[clippy::manual_string_new]"
        );
    }

    #[test]
    fn allow_exact_drops_matching_diagnostic_at_ingestion() {
        // The sited allow acts after clippy has spoken: even an error-level
        // diagnostic (the `#[expect]`-sibling shape that defeats `-A`) is
        // filtered when lint and file both match. A bare lint name in config
        // matches the `clippy::`-qualified code cargo emits.
        let input = json_compiler_message(
            "error",
            Some("clippy::unused_async_trait_impl"),
            "unused `async` for async trait impl function",
            "crates/system/src/kernel.rs",
            747,
            5,
        );
        let sited =
            [SitedAllow::parse("unused_async_trait_impl@crates/system/src/kernel.rs").unwrap()];
        let parsed = parse_clippy_from_json(&input, false, &sited);
        assert!(parsed.diagnostics.is_empty());
        assert!(!parsed.parse_failed);
    }

    #[test]
    fn allow_exact_is_file_scoped_not_workspace_wide() {
        // Same lint in a different file must survive - the whole point of the
        // sited form over `allow`.
        let mut input = json_compiler_message(
            "warning",
            Some("clippy::unused_async"),
            "unused `async`",
            "src/a.rs",
            1,
            1,
        );
        input.push('\n');
        input.push_str(&json_compiler_message(
            "warning",
            Some("clippy::unused_async"),
            "unused `async`",
            "src/b.rs",
            2,
            2,
        ));
        let sited = [SitedAllow::parse("clippy::unused_async@src/a.rs").unwrap()];
        let parsed = parse_clippy_from_json(&input, false, &sited);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(
            parsed.diagnostics[0].location.as_deref(),
            Some("src/b.rs:2:2")
        );
    }

    #[test]
    fn allow_exact_does_not_touch_other_lints_in_the_file() {
        let input = json_compiler_message(
            "warning",
            Some("clippy::needless_return"),
            "unneeded return statement",
            "src/a.rs",
            3,
            5,
        );
        let sited = [SitedAllow::parse("clippy::unused_async@src/a.rs").unwrap()];
        let parsed = parse_clippy_from_json(&input, false, &sited);
        assert_eq!(parsed.diagnostics.len(), 1);
    }

    #[test]
    fn json_to_clippy_sets_parse_failed_when_sweep_failed_with_no_events() {
        // cargo crashed before producing any compiler-message events.
        let parsed = parse_clippy_from_json("", true, &[]);
        assert!(parsed.parse_failed);
        assert!(parsed.diagnostics.is_empty());
    }

    #[test]
    fn json_to_clippy_no_parse_failed_when_sweep_succeeded() {
        // Empty stdout but successful exit (clean compile). Not a parse
        // failure - just nothing to report.
        let parsed = parse_clippy_from_json("", false, &[]);
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
    fn format_override_is_refused_on_the_final_argv() {
        let own = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<String>>();
        // brokkr's own injected pair, alone: fine.
        assert!(
            reject_format_override(&own(&["-Z", "unstable-options", "--format", "json"])).is_ok()
        );
        // A forwarded override after it wins in libtest, so it must be refused -
        // this is the case that silently degraded the per-test cap to the idle
        // ceiling.
        assert!(
            reject_format_override(&own(&["--format", "json", "--format", "pretty"])).is_err()
        );
        // The `=` spelling is one argv entry and was missed by an equality check.
        assert!(reject_format_override(&own(&["--format", "json", "--format=pretty"])).is_err());
        assert!(reject_format_override(&own(&["--format=pretty"])).is_err());
        // Unrelated args are untouched.
        assert!(reject_format_override(&own(&["--nocapture", "--test-threads=1"])).is_ok());
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
            &[],
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
            &[],
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
            &[],
        );
        assert_eq!(get(&env, "BROKKR_TEST_BIN_DIR"), Some("/meta/target/debug"));
        assert!(get(&env, "CARGO_TARGET_DIR").is_none());
        assert!(rustflags_value(&env).is_none());
    }

    #[test]
    fn sweep_cargo_env_omits_bin_dir() {
        let key = crate::config::rustflags_target_key(&rustflags_sweep().rustflags).unwrap();
        let env = sweep_cargo_env(&rustflags_sweep(), Path::new("/meta/target"), &[]);
        // Clippy shares the same isolated dir the test phase builds into.
        assert_eq!(
            get(&env, "CARGO_TARGET_DIR"),
            Some(format!("/meta/target/rustflags-{key}").as_str())
        );
        assert!(rustflags_value(&env).is_some());
        // Clippy has no test binary to spawn.
        assert!(get(&env, "BROKKR_TEST_BIN_DIR").is_none());
        // A plain sweep contributes nothing.
        assert!(
            sweep_cargo_env(&ResolvedSweep::default(), Path::new("/meta/target"), &[]).is_empty()
        );
        // A plain sweep with env-sink lint allows carries them and nothing else.
        let allows = vec!["-Aclippy::assert_is_empty".to_owned()];
        let env = sweep_cargo_env(&ResolvedSweep::default(), Path::new("/meta/target"), &allows);
        assert!(get(&env, "CARGO_TARGET_DIR").is_none());
        assert!(rustflags_value(&env)
            .expect("allow flags reach the env")
            .contains("-Aclippy::assert_is_empty"));
    }

    fn release_sweep() -> ResolvedSweep {
        ResolvedSweep {
            label: "timing".into(),
            profile: Some(crate::config::SweepProfile::Release),
            ..Default::default()
        }
    }

    #[test]
    fn a_pinned_profile_reaches_every_compiling_path() {
        // The invariant the key stands on: clippy, the test invocation, the
        // process-isolated selection and the coverage enumeration all compile
        // the same sweep, and a profile that reached only some of them would
        // mean linting one build, running another, and auditing a third.
        let sweep = release_sweep();
        assert_eq!(sweep_profile_args(&sweep), ["--release"]);

        let test_args = sweep_selection_args(&sweep, &Selection::Bare);
        assert_eq!(test_args.first().map(String::as_str), Some("--release"));

        let clippy = clippy_args(&sweep, &Selection::Bare, &[]);
        assert!(clippy.iter().any(|a| a == "--release"), "got: {clippy:?}");

        let enumeration = shape_selection_args(&sweep);
        assert_eq!(enumeration.first().map(String::as_str), Some("--release"));

        // `--release` must not read as a target selector - that would suppress
        // the `--tests` scoping and let doctests back into the sweep.
        assert!(!has_target_selector(&test_args));
    }

    #[test]
    fn a_pinned_unification_reaches_every_compiling_path() {
        // The twin of the profile invariant above, and the one that shipped
        // broken: the ordinary test path never emitted the pin, so a lane was
        // labelled `unification package`, isolated into its own target dir,
        // and then compiled under workspace resolution. It ran 839 green tests
        // against a defect it was created to catch. Assert the flags on every
        // compiling path, not the label.
        use crate::config::{CargoUnification, EffectiveUnification};
        let pkg = ResolvedSweep {
            packages: vec!["daemon".to_owned()],
            effective_unification: EffectiveUnification::Pinned(CargoUnification::Package),
            ..ResolvedSweep::default()
        };
        let pin = "resolver.feature-unification=\"package\"";

        let daemon = Selection::Explicit(vec!["daemon".to_owned()]);
        let test_args = sweep_selection_args(&pkg, &daemon);
        assert!(test_args.iter().any(|a| a == "-Zfeature-unification"), "test: {test_args:?}");
        assert!(test_args.iter().any(|a| a == pin), "test: {test_args:?}");

        let clippy = clippy_args(&pkg, &daemon, &[]);
        assert!(clippy.iter().any(|a| a == pin), "clippy: {clippy:?}");

        let enumeration = shape_selection_args(&pkg);
        assert!(enumeration.iter().any(|a| a == pin), "audit: {enumeration:?}");

        // The `build_packages` pre-build and the rustdoc phase compile the
        // sweep too; both once went out without the pin.
        let pre_build = pre_build_args(&pkg, "daemon", &[]);
        assert!(pre_build.iter().any(|a| a == pin), "pre-build: {pre_build:?}");
        // And it reports what it built, so a complete plan can hash it.
        assert!(pre_build.iter().any(|a| a == "--message-format=json-render-diagnostics"), "{pre_build:?}");

        let doc = doc_args(&pkg, &daemon, &crate::config::RustdocConfig::default());
        assert!(doc.iter().any(|a| a == pin), "rustdoc: {doc:?}");

        // The pin is a cargo option, so it must land before the `--` split or
        // libtest receives it.
        if let Some(split) = test_args.iter().position(|a| a == "--") {
            let z = test_args.iter().position(|a| a == "-Zfeature-unification");
            assert!(z.is_some_and(|i| i < split), "pin after `--`: {test_args:?}");
        }
    }

    /// A pre-build's stream yields its executables - a dependency's lib and
    /// build script are not support executables - and their fingerprint
    /// follows content.
    #[test]
    fn a_pre_build_reports_its_executables() {
        let dir = crate::test_scratch::scratch("output", "support_artifacts");
        let exe = dir.join("server");
        std::fs::write(&exe, b"v1").unwrap();
        let stdout = format!(
            "{}\n{}\n{}\n{}\n",
            r#"{"reason":"compiler-artifact","target":{"name":"dep","kind":["lib"]},"executable":null}"#,
            r#"{"reason":"compiler-artifact","target":{"name":"build-script-build","kind":["custom-build"]},"executable":"/t/build/x/build-script-build"}"#,
            format_args!(
                r#"{{"reason":"compiler-artifact","target":{{"name":"server","kind":["bin"]}},"executable":"{}"}}"#,
                exe.display()
            ),
            r#"{"reason":"build-finished","success":true}"#,
        );
        let arts = support_artifacts(&stdout, "server-pkg");
        assert_eq!(arts.len(), 1, "{arts:?}");
        assert_eq!(arts[0].target, "server");
        let before = support_fingerprint(&arts).unwrap();
        std::fs::write(&exe, b"v2").unwrap();
        assert_ne!(before, support_fingerprint(&arts).unwrap());
    }

    #[test]
    fn an_unpinned_sweep_is_untouched() {
        // Every repo that never sets the key must produce byte-identical argv
        // to what it produced before the key existed.
        let plain = ResolvedSweep::default();
        assert!(sweep_profile_args(&plain).is_empty());
        assert!(!sweep_selection_args(&plain, &Selection::Bare).iter().any(|a| a == "--release"));
        assert!(!clippy_args(&plain, &Selection::Bare, &[]).iter().any(|a| a == "--release"));
        assert!(!shape_selection_args(&plain).iter().any(|a| a == "--release"));
        // Same for unification: `auto` un-promoted pins nothing anywhere.
        for args in [
            sweep_selection_args(&plain, &Selection::Bare),
            clippy_args(&plain, &Selection::Bare, &[]),
            shape_selection_args(&plain),
            pre_build_args(&plain, "bin", &[]),
        ] {
            assert!(!args.iter().any(|a| a == "-Zfeature-unification"), "got: {args:?}");
        }
    }

    /// rustc knows no `cargo` lint tool: a `cargo::` allow is applied at
    /// ingestion, never passed to clippy as `-A`.
    #[test]
    fn clippy_args_leave_cargo_lints_out_of_the_allow_flags() {
        let allow = ["clippy::unused_async".to_owned(), "cargo::unused_dependencies".to_owned()];
        let args = clippy_args(&ResolvedSweep::default(), &Selection::Bare, &allow);
        assert!(args.iter().any(|a| a == "clippy::unused_async"), "{args:?}");
        assert!(!args.iter().any(|a| a.starts_with("cargo::")), "{args:?}");
    }

    /// A `cargo::` `allow_exact` entry never reaches a build, so the verdict
    /// line does not claim it widens there - not even beside entries that do.
    #[test]
    fn the_verdict_line_calls_cargo_allow_exact_entries_sited() {
        let sited = |s: &str| SitedAllow::parse(s).unwrap();
        let cargo_only = verdict_context(&None, 1, &[], &[sited("cargo::unused_dependencies@a/Cargo.toml")]);
        assert!(cargo_only.contains("allow_exact: cargo::unused_dependencies (sited)"), "{cargo_only}");
        let mixed = verdict_context(
            &None,
            1,
            &[],
            &[sited("cargo::unused_dependencies@a/Cargo.toml"), sited("dead_code@src/a.rs")],
        );
        assert!(
            mixed.contains("allow_exact: dead_code, cargo::unused_dependencies (sited)"),
            "{mixed}"
        );
    }

    #[test]
    fn the_shape_line_names_a_pinned_profile() {
        // Same reason `rustflags` is surfaced: it redirects the sweep to
        // another target subdirectory and buys a full recompile there, and an
        // unexplained rebuild is what the collapsed log must not hide.
        let line = describe_sweep(&release_sweep(), true, &Selection::Bare);
        assert!(line.contains("profile release"), "got: {line}");
        // An unpinned sweep says nothing - it is the command's default.
        assert!(!describe_sweep(&ResolvedSweep::default(), true, &Selection::Bare).contains("profile"));
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
            completeness: cargo_filter::Completeness::Complete,
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
        // No package flags: cargo's default selection, never called the
        // workspace.
        assert_eq!(describe_sweep(&sweep("default"), false, &Selection::Bare), "default selection");

        // The sweep's own `packages` list; one package is named.
        let scoped = ResolvedSweep {
            packages: s(&["nautilus-core", "nautilus-model"]),
            ..sweep("ffi")
        };
        let own = Selection::configured(&scoped, SelectionPhase::Clippy);
        assert_eq!(describe_sweep(&scoped, false, &own), "2 pkgs");
        assert_eq!(describe_sweep(&scoped, false, &Selection::Explicit(s(&["nautilus-core"]))), "-p nautilus-core");

        // `--workspace --exclude` is a test-phase-only shape; the clippy line
        // describes the clippy selection, matching what actually runs.
        let excluded = ResolvedSweep {
            test_exclude_packages: s(&["nautilus-pyo3", "nautilus-cli"]),
            ..sweep("default")
        };
        let clippy = Selection::configured(&excluded, SelectionPhase::Clippy);
        assert_eq!(describe_sweep(&excluded, false, &clippy), "default selection");
        let test = Selection::configured(&excluded, SelectionPhase::Test);
        assert_eq!(
            describe_sweep(&excluded, true, &test),
            "workspace -2 pkgs, serial"
        );
    }

    #[test]
    fn describe_sweep_reads_features_back_out_of_argv() {
        let all = ResolvedSweep {
            cargo_feature_args: s(&["--all-features"]),
            ..sweep("all")
        };
        assert_eq!(describe_sweep(&all, false, &Selection::Bare), "default selection, all-features");

        let consumer = ResolvedSweep {
            cargo_feature_args: s(&["--no-default-features", "--features", "commands"]),
            ..sweep("consumer")
        };
        assert_eq!(
            describe_sweep(&consumer, false, &Selection::Bare),
            "default selection, no-default +commands"
        );

        // The `--features=x,y` spelling is equivalent.
        let joined = ResolvedSweep {
            cargo_feature_args: s(&["--features=ffi,live"]),
            ..sweep("j")
        };
        assert_eq!(describe_sweep(&joined, false, &Selection::Bare), "default selection, +ffi,live");
    }

    #[test]
    fn describe_sweep_does_not_restate_the_label() {
        // The legacy no-`[[check]]` path names its synthesized sweep after the
        // feature shape, which would otherwise print twice on one line.
        let legacy = ResolvedSweep {
            cargo_feature_args: s(&["--all-features"]),
            ..sweep("all-features")
        };
        assert_eq!(describe_sweep(&legacy, false, &Selection::Bare), "default selection");
    }

    #[test]
    fn describe_sweep_surfaces_rustflags_and_isolation() {
        // rustflags silently redirect the sweep to its own target dir; the
        // collapsed form must not hide the cause of a full recompile.
        let sim = ResolvedSweep {
            rustflags: s(&["--cfg", "madsim"]),
            ..sweep("sim")
        };
        assert!(describe_sweep(&sim, false, &Selection::Bare).contains("rustflags --cfg madsim"));
        assert!(describe_sweep(&sim, false, &Selection::Bare).contains("isolated target"));
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
            describe_sweep(&tier, true, &Selection::Bare),
            "default selection, 3 skips, include-ignored, parallel"
        );
        // Clippy never takes libtest filters, so its line omits them.
        assert_eq!(describe_sweep(&tier, false, &Selection::Bare), "default selection");
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
            describe_sweep(&one, true, &Selection::Bare),
            "default selection, --test cli_sort, serial"
        );

        // Two filters stay two distinct items, each self-contained.
        let two = ResolvedSweep {
            cargo_test_filters: s(&["--test", "cli_sort", "--test", "cli_env"]),
            ..sweep("sort")
        };
        assert_eq!(
            describe_sweep(&two, true, &Selection::Bare),
            "default selection, --test cli_sort, --test cli_env, serial"
        );

        // Clippy never takes cargo test filters, so its line omits them.
        assert_eq!(describe_sweep(&one, false, &Selection::Bare), "default selection");
    }

    #[test]
    fn describe_sweep_thread_policy_tracks_watchdog_lane() {
        // None and Some(1) both mean the serial per-test watchdog lane.
        for threads in [None, Some(1)] {
            let serial = ResolvedSweep {
                test_threads: threads,
                ..sweep("serial")
            };
            assert_eq!(describe_sweep(&serial, true, &Selection::Bare), "default selection, serial");
        }
        for threads in [Some(0), Some(4)] {
            let parallel = ResolvedSweep {
                test_threads: threads,
                ..sweep("par")
            };
            assert_eq!(describe_sweep(&parallel, true, &Selection::Bare), "default selection, parallel");
        }
    }

    /// One sweep's entry in one `check` phase under a CLI `-p` set.
    fn entry(sweep: &ResolvedSweep, phase: SelectionPhase, cli: &[&str]) -> LaneEntry {
        let sel = PhaseSelection::for_check(phase, std::slice::from_ref(sweep), &s(cli)).unwrap();
        sel.entry(0).unwrap().clone()
    }

    fn override_of(packages: &[&str]) -> Selection {
        Selection::Override { packages: s(packages), provenance: crate::check_cmd::selection::Provenance::Cli }
    }

    #[test]
    fn cli_package_replaces_sweep_selection_or_skips() {
        // Default-selection sweep: `-p` applies; without `-p` the sweep stands.
        let plain = sweep("default");
        assert_eq!(entry(&plain, SelectionPhase::Test, &["x"]).attempt().unwrap().selection(), &override_of(&["x"]));
        assert_eq!(entry(&plain, SelectionPhase::Test, &[]).attempt().unwrap().selection(), &Selection::Bare);

        // The exclusion list rules the package out - but only for the test
        // phase; clippy ignores test_exclude_packages by design.
        let excluded = ResolvedSweep {
            test_exclude_packages: s(&["x"]),
            ..sweep("default")
        };
        assert!(matches!(entry(&excluded, SelectionPhase::Test, &["x"]), LaneEntry::Excluded(_)));
        assert_eq!(
            entry(&excluded, SelectionPhase::Clippy, &["x"]).attempt().unwrap().selection(),
            &override_of(&["x"])
        );

        // A `packages` list admits members and skips everything else.
        let scoped = ResolvedSweep {
            packages: s(&["a", "b"]),
            ..sweep("ffi")
        };
        assert_eq!(entry(&scoped, SelectionPhase::Test, &["a"]).attempt().unwrap().selection(), &override_of(&["a"]));
        assert!(matches!(entry(&scoped, SelectionPhase::Clippy, &["x"]), LaneEntry::Excluded(_)));
    }

    #[test]
    fn cli_package_set_intersects_per_sweep() {
        // A multi `-p` set runs the in-scope subset with a note per dropped
        // package; the sweep is excluded only when nothing survives.
        let scoped = ResolvedSweep {
            packages: s(&["a", "b"]),
            ..sweep("ffi")
        };
        let mixed = entry(&scoped, SelectionPhase::Test, &["a", "x"]);
        assert_eq!(mixed.attempt().unwrap().selection(), &override_of(&["a"]));
        assert_eq!(mixed.notes().len(), 1);
        assert!(mixed.notes()[0].to_string().contains("-p x"));
        assert!(matches!(entry(&scoped, SelectionPhase::Test, &["x", "y"]), LaneEntry::Excluded(_)));

        // Test-phase exclusion drops per package too.
        let excluded = ResolvedSweep {
            test_exclude_packages: s(&["x"]),
            ..sweep("default")
        };
        let mixed = entry(&excluded, SelectionPhase::Test, &["a", "x"]);
        assert_eq!(mixed.attempt().unwrap().selection(), &override_of(&["a"]));
        assert_eq!(mixed.notes().len(), 1);
    }

    #[test]
    fn cli_package_excluded_from_doc_only_sweep_skips_the_lane() {
        // A documented contract, not an accident: downstream configs close a
        // bin-only crate's doctest failure with `test_exclude_packages` on a
        // `doc_only` sweep, and `check -p <that crate>` must then skip the
        // doctest lane rather than run `cargo test --doc -p <crate>`.
        let doctests = ResolvedSweep {
            doc_only: true,
            test_exclude_packages: s(&["bin-only"]),
            ..sweep("doctests")
        };
        assert!(matches!(entry(&doctests, SelectionPhase::Test, &["bin-only"]), LaneEntry::Excluded(_)));
        let mixed = entry(&doctests, SelectionPhase::Test, &["bin-only", "lib"]);
        assert_eq!(mixed.attempt().unwrap().selection(), &override_of(&["lib"]));
        assert_eq!(mixed.notes().len(), 1);
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
        assert_eq!(describe_sweep(&excluded, true, &override_of(&["x"])), "-p x, serial");
        assert_eq!(describe_sweep(&excluded, false, &override_of(&["x"])), "-p x");
        assert_eq!(describe_sweep(&excluded, false, &override_of(&["x", "y"])), "-p x -p y");
    }

}

#[cfg(test)]
mod serial_observe_tests {
    use super::*;
    use crate::test_runner::{ObsEvent, Observation, StreamSource};

    fn plan(strict: bool) -> (SerialPlan, BinaryUnit) {
        let b = test_binary_for_tests("core", "test", "suite");
        let unit = BinaryUnit::of(&b);
        let plan = SerialPlan {
            resolution: None,
            units: HashMap::from([(b.executable.clone(), unit.clone())]),
            hashes: HashMap::new(),
            expected: Vec::new(),
            rustdoc: false,
            strict,
        };
        (plan, unit)
    }

    fn hear(observe: &SerialObserve<'_>, executable: &str) {
        (observe.sink())(Observation {
            stream: 1,
            source: StreamSource::Harness { executable: executable.into() },
            event: ObsEvent::Started { name: "t".into() },
        });
    }

    /// A lane with an inventory attributes its harness streams to the planned
    /// binaries whether or not the run is certifying: the plan attributes, and
    /// only a strict plan also refuses. A harness the plan does not hold is an
    /// unattributed stream, not an attribution to the wrong binary.
    #[test]
    fn a_plan_attributes_harness_streams_without_being_strict() {
        let tap = LaneTap::new(41_001);
        let (p, unit) = plan(false);
        let observe = SerialObserve { tap: &tap, plan: Some(p) };
        hear(&observe, "/t/debug/deps/suite-1");
        assert!(tap.seen_units().contains(&unit), "the planned harness is attributed to its binary");
        let other = LaneTap::new(41_002);
        let (p, _) = plan(false);
        let observe = SerialObserve { tap: &other, plan: Some(p) };
        hear(&observe, "/t/debug/deps/unknown-9");
        assert!(other.seen_units().is_empty(), "an unplanned harness is not attributed");
    }
}

