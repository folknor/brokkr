use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::cargo::metadata::{self, CargoMetadata, Query};
use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// CargoProfile
// ---------------------------------------------------------------------------

/// Identifies how a benchmarked binary was built. Stored on every result row
/// (column `cargo_profile`) so Rust-built runs stay distinguishable from
/// external baselines that go through entirely different build systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CargoProfile {
    /// Rust `cargo build --release` - the standard brokkr-built binary.
    #[default]
    Release,
    /// Rust `cargo build` under the dev profile - a harness whose config or
    /// `--debug` selects a debug build (ratatoskr's sync bench, piners'
    /// measured corpus). Labelled `release` before this variant existed, so
    /// older dev rows in those series carry the wrong profile.
    Dev,
    /// External Java/Maven build (Planetiler baseline).
    Java,
    /// External CMake/C++ build (Tilemaker baseline).
    CMake,
}

impl CargoProfile {
    /// The Rust profile a `debug` flag selects - the one mapping every
    /// harness-built writer should use rather than spelling `Release`.
    pub fn for_debug(debug: bool) -> Self {
        if debug { Self::Dev } else { Self::Release }
    }

    pub fn as_label(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Dev => "dev",
            Self::Java => "java",
            Self::CMake => "cmake",
        }
    }

    /// Cargo's profile directory for Rust-built artifacts. External build
    /// kinds have no Cargo target directory.
    pub fn target_dir(self) -> Option<&'static str> {
        match self {
            Self::Release => Some("release"),
            Self::Dev => Some("debug"),
            Self::Java | Self::CMake => None,
        }
    }

    /// Parse a DB-stored value. Returns `Err` on unknown strings so the
    /// caller can surface a migration typo instead of silently coercing
    /// to `Release`.
    pub fn from_db(s: &str) -> Result<Self, UnknownCargoProfile> {
        match s {
            "release" => Ok(Self::Release),
            "dev" => Ok(Self::Dev),
            "java" => Ok(Self::Java),
            "cmake" => Ok(Self::CMake),
            other => Err(UnknownCargoProfile(other.to_owned())),
        }
    }

    fn is_rust_build(self) -> bool {
        matches!(self, Self::Dev | Self::Release)
    }
}

#[derive(Debug)]
pub struct UnknownCargoProfile(pub String);

/// The executable Cargo actually produced for a build request, together with
/// the build settings needed to describe that artifact on a measurement row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltArtifact {
    pub path: PathBuf,
    pub profile: CargoProfile,
    /// Explicit feature requests passed to this Cargo invocation.
    pub features: Vec<String>,
    /// Cargo target name reported with the executable artifact.
    pub target: String,
}

// ---------------------------------------------------------------------------
// Build configuration
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct BuildConfig {
    pub package: Option<String>,
    pub bin: Option<String>,
    pub example: Option<String>,
    pub features: Vec<String>,
    pub default_features: bool,
    /// `--all-features`. Only `brokkr run --commit` sets it, forwarding the
    /// flag a plain `run` hands to `cargo run`.
    pub all_features: bool,
    pub profile: CargoProfile,
}

impl BuildConfig {
    pub fn release(package: Option<&str>) -> Self {
        Self {
            package: package.map(std::borrow::ToOwned::to_owned),
            bin: None,
            example: None,
            features: Vec::new(),
            default_features: true,
            all_features: false,
            profile: CargoProfile::Release,
        }
    }

    pub fn release_with_features(package: Option<&str>, features: &[&str]) -> Self {
        Self {
            package: package.map(std::borrow::ToOwned::to_owned),
            bin: None,
            example: None,
            features: features.iter().map(|s| (*s).to_owned()).collect(),
            default_features: true,
            all_features: false,
            profile: CargoProfile::Release,
        }
    }

    /// Release build with features from owned strings (e.g. host config features).
    pub fn release_with_owned_features(package: Option<&str>, features: &[String]) -> Self {
        Self {
            package: package.map(std::borrow::ToOwned::to_owned),
            bin: None,
            example: None,
            features: features.to_vec(),
            default_features: true,
            all_features: false,
            profile: CargoProfile::Release,
        }
    }

    pub fn release_no_defaults(package: Option<&str>, features: &[&str]) -> Self {
        Self {
            package: package.map(std::borrow::ToOwned::to_owned),
            bin: None,
            example: None,
            features: features.iter().map(|s| (*s).to_owned()).collect(),
            default_features: false,
            all_features: false,
            profile: CargoProfile::Release,
        }
    }

    /// Build config for an external harness crate described by a `[*.harness]`
    /// block (`package` + optional `binary`/`features`), under an
    /// already-resolved profile. Shared by the ratatoskr and piners harness
    /// builds and the measured corpus path (which appends the hotpath feature
    /// to the returned config before building) - they build the same kind of
    /// engine, so the config mapping lives once here.
    pub fn for_harness_with_profile(
        harness_cfg: &crate::config::HarnessConfig,
        profile: CargoProfile,
    ) -> Self {
        Self {
            package: Some(harness_cfg.package.clone()),
            bin: harness_cfg.binary.clone(),
            example: None,
            features: harness_cfg.features.clone(),
            default_features: true,
            all_features: false,
            profile,
        }
    }
}

/// Target directory override shared by Cargo build, metadata, binary lookup,
/// and benchmark paths for a brokkr-created worktree.
pub(crate) fn cargo_target_dir_override(build_root: &Path) -> Option<String> {
    crate::build_purpose::target_dir_override(build_root)
}

fn target_dir_env(target_dir: Option<&str>) -> Vec<(&'static str, &str)> {
    target_dir.map_or_else(Vec::new, |dir| vec![("CARGO_TARGET_DIR", dir)])
}

// ---------------------------------------------------------------------------
// Project info
// ---------------------------------------------------------------------------

pub struct ProjectInfo {
    pub target_dir: PathBuf,
    /// Whether a bare (no `-p`) cargo selection selects every workspace
    /// member: `workspace_default_members` equals `workspace_members`. False
    /// for a `[workspace] default-members` subset and for a non-virtual
    /// workspace whose root package is the default. The parallel test lane
    /// reads this to decide whether workspace-mode feature unification is a
    /// faithful description of its own prebuild - see
    /// `check_cmd/parallel.rs`.
    pub bare_selection_is_whole_workspace: bool,
    /// Every workspace member, package id -> package name. The ids are in the
    /// form cargo's JSON messages carry as `package_id`: how `check` tells a
    /// workspace diagnostic from a dependency's, and names the package a
    /// diagnostic belongs to.
    pub workspace_members: std::collections::HashMap<String, String>,
    /// Package ids a bare (no `-p`) selection builds.
    pub default_members: std::collections::HashSet<String>,
    /// The workspace root: what cargo's manifest-lint paths are relative to,
    /// whichever directory it ran in.
    pub workspace_root: PathBuf,
    /// Every member's `Cargo.toml` (absolute, as `cargo metadata` reports it)
    /// -> its package id. How a manifest lint, which names a file and no
    /// package, is attributed to one.
    pub member_manifests: std::collections::HashMap<PathBuf, String>,
}

// ---------------------------------------------------------------------------
// project_info
// ---------------------------------------------------------------------------

/// Resolve project facts from the shared typed `cargo metadata` snapshot.
///
/// Runs `cargo metadata` in the given directory (or cwd if `None`).
pub fn project_info(cwd: Option<&Path>) -> Result<ProjectInfo, DevError> {
    let project_root = match cwd {
        Some(p) => p.to_owned(),
        None => std::env::current_dir()
            .map_err(|e| DevError::Build(format!("cannot determine current directory: {e}")))?,
    };

    let target_override = cargo_target_dir_override(&project_root);
    let env = target_dir_env(target_override.as_deref());
    let metadata = metadata::load(&project_root, Query::workspace(), &env, None)?;

    Ok(ProjectInfo {
        target_dir: PathBuf::from(&metadata.target_directory),
        bare_selection_is_whole_workspace: metadata.members_are_all_default(),
        workspace_members: metadata.workspace_member_name_map(),
        default_members: metadata.default_member_ids().into_iter().collect(),
        workspace_root: PathBuf::from(&metadata.workspace_root),
        member_manifests: metadata.member_manifests(),
    })
}

// ---------------------------------------------------------------------------
// cargo build
// ---------------------------------------------------------------------------

/// Build a crate and return the compiled executable and its build identity.
///
/// Parses `--message-format=json` output to find the `"executable"` field.
///
/// Refuses unless this process holds the brokkr lock - see
/// [`cargo_build_observed`].
pub fn cargo_build(config: &BuildConfig, project_root: &Path) -> Result<BuiltArtifact, DevError> {
    cargo_build_observed(
        config,
        project_root,
        None,
        crate::shutdown::ChildIsolation::Inherit,
    )
}

/// Run a Cargo command rooted at the detected checkout, including Cargo's
/// configured environment and the isolated target directory for worktrees.
/// The lock's child PID is published for the command's lifetime.
pub fn run_cargo_for_root(
    args: &[&str],
    build_root: &crate::project::BuildRoot,
    env: &[(&str, &str)],
    on_spawn: Option<&dyn Fn(u32)>,
    isolation: crate::shutdown::ChildIsolation<'_>,
) -> Result<output::CapturedOutput, DevError> {
    with_published_cargo(on_spawn, |publish| {
        let target_override = cargo_target_dir_override(build_root.as_path());
        let mut cargo_env = env.to_vec();
        cargo_env.extend(target_dir_env(target_override.as_deref()));
        if let Some(dir) = &target_override {
            output::detail_or_console(&format!("isolating worktree Cargo target dir -> {dir}"));
        }
        output::run_captured_observed(
            "cargo",
            args,
            build_root.as_path(),
            &cargo_env,
            Some(publish),
            isolation,
            output::SpawnProfile::Cargo { phase: None },
        )
    })
}

/// As [`cargo_build`], but takes an extra `on_spawn` observer that fires with
/// the cargo PID, and an isolation policy. Process-group isolation borrows the
/// caller's live `SigtermGuard`, the proof that terminal signals bridge to
/// cargo's process group.
///
/// **Every build runs under the lock, and publishes its cargo PID into it.**
/// The hold is looked up here ([`crate::lockfile::current_hold`]) and a build
/// with none is refused: an unlocked build competes with a measurement, and on
/// a guarded host it fails anyway at cargo's first rustc probe, in words that
/// never mention the lock. Checking at this one choke point covers call paths
/// several layers below the command that took the lock, where threading a
/// `&LockGuard` down would not. The cargo PID goes into the lock file for the
/// build's duration, so `brokkr kill --hard` during a build reaches cargo;
/// before, only ratatoskr's orchestration published it, via `on_spawn`.
pub fn cargo_build_observed(
    config: &BuildConfig,
    project_root: &Path,
    on_spawn: Option<&dyn Fn(u32)>,
    isolation: crate::shutdown::ChildIsolation<'_>,
) -> Result<BuiltArtifact, DevError> {
    with_published_cargo(on_spawn, |publish| {
        cargo_build_locked(config, project_root, publish, isolation)
    })
}

fn with_published_cargo<T>(
    on_spawn: Option<&dyn Fn(u32)>,
    run: impl FnOnce(&dyn Fn(u32)) -> Result<T, DevError>,
) -> Result<T, DevError> {
    let hold = crate::lockfile::current_hold().ok_or_else(|| {
        DevError::Lock(
            "refusing to run cargo outside the brokkr lock - this is a brokkr bug: \
             the command that reached this Cargo spawn must take the lock first"
                .into(),
        )
    })?;
    let publish = |pid: u32| {
        hold.set_child_pid(pid);
        if let Some(cb) = on_spawn {
            cb(pid);
        }
    };
    let result = run(&publish);
    hold.clear_child_pid();
    result
}

/// Body of [`cargo_build_observed`], run once the hold is established.
fn cargo_build_locked(
    config: &BuildConfig,
    project_root: &Path,
    on_spawn: &dyn Fn(u32),
    isolation: crate::shutdown::ChildIsolation<'_>,
) -> Result<BuiltArtifact, DevError> {
    if !config.profile.is_rust_build() {
        return Err(DevError::Build(format!(
            "{} is not a Cargo build profile",
            config.profile.as_label()
        )));
    }
    let profile = config.profile;
    let on_spawn = Some(on_spawn);
    let args = build_args(config)?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    output::build_msg(&format!("cargo {}", arg_refs.join(" ")));

    // Isolate worktree builds into their own target dir. On hosts that point
    // every checkout at a shared cargo target dir (e.g. a parent
    // `.cargo/config.toml` with `build.target-dir`), a `--commit` worktree
    // build would otherwise write its artifacts and fingerprints into the SAME
    // target the main tree reads from. Because the worktree's fingerprints
    // post-date the main tree's source mtimes, a later main-tree build then
    // judges everything "up to date" and re-links the stale worktree binary -
    // benchmarking old code under the new commit's hash. Pinning
    // `CARGO_TARGET_DIR` to a per-worktree dir keeps the two builds' target
    // trees apart; the env var overrides any inherited `build.target-dir`.
    // That dir sits under the live project's target, on its disk (`worktree`'s
    // module doc).
    let target_override = cargo_target_dir_override(project_root);
    let env = target_dir_env(target_override.as_deref());
    if let Some(dir) = &target_override {
        output::detail_or_console(&format!("isolating worktree build target dir -> {dir}"));
    }

    let start = Instant::now();
    let captured = output::run_captured_observed(
        "cargo",
        &arg_refs,
        project_root,
        &env,
        on_spawn,
        isolation,
        output::SpawnProfile::Cargo { phase: None },
    )?;
    let elapsed = start.elapsed();

    if !captured.success() {
        // Everything cargo had to say is printed here, once; the error
        // returned is a bare label so `main` does not print it a second time.
        dump_compiler_messages(&captured.stdout);
        dump_build_stderr(&captured.stderr);
        return Err(DevError::Reported("cargo build failed".into()));
    }

    // An `--example` build names its target as surely as `--bin` does, and
    // cargo emits the example's executable under that name. Without it here
    // the example path fell through to `find_executable`'s "exactly one
    // executable" fallback - correct in practice, but by luck rather than by
    // the name we already know.
    let expected = config
        .bin
        .as_deref()
        .or(config.example.as_deref())
        .or(config.package.as_deref());
    let executable = find_executable_artifact(&captured.stdout, expected)?;
    let built = BuiltArtifact {
        path: executable.path,
        profile,
        features: config.features.clone(),
        target: executable.target,
    };

    output::build_msg(&format!(
        "done in {} -> {}",
        output::format_duration(elapsed),
        built.path.display(),
    ));

    Ok(built)
}

/// Resolve the selected profile's binary path from cargo metadata without
/// building.
pub fn resolve_existing_binary(
    config: &BuildConfig,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    if !config.profile.is_rust_build() {
        return Err(DevError::Build(format!(
            "{} is not a Cargo build profile",
            config.profile.as_label()
        )));
    }
    let target_override = cargo_target_dir_override(project_root);
    let env = target_dir_env(target_override.as_deref());
    let metadata = metadata::load(project_root, Query::workspace(), &env, None)?;
    let bin_name = resolve_bin_name(&metadata, config)?;

    let mut path = PathBuf::from(&metadata.target_directory);
    path.push(config.profile.target_dir().ok_or_else(|| {
        DevError::Build(format!(
            "{} has no Cargo target directory",
            config.profile.as_label()
        ))
    })?);
    path.push(bin_name);

    if !path.exists() {
        return Err(DevError::Build(format!(
            "binary not found at {} (build first or omit --no-build)",
            path.display()
        )));
    }

    Ok(path)
}

/// Assemble the argument list for `cargo build`.
fn build_args(config: &BuildConfig) -> Result<Vec<String>, DevError> {
    if !config.profile.is_rust_build() {
        return Err(DevError::Build(format!(
            "{} is not a Cargo build profile",
            config.profile.as_label()
        )));
    }
    let shape = crate::build_purpose::resolve(crate::build_purpose::Request {
        purpose: crate::build_purpose::Purpose::Build,
        profiles: crate::build_purpose::Profiles {
            invocation: Some(config.profile == CargoProfile::Dev),
            ..Default::default()
        },
        packages: config
            .package
            .as_ref()
            .map_or_else(Vec::new, |pkg| vec!["-p".into(), pkg.clone()]),
        features: crate::build_purpose::Features::Joined {
            values: &config.features,
            all: config.all_features,
            no_default: !config.default_features,
        },
        unification: Vec::new(),
        targets: config
            .bin
            .iter()
            .flat_map(|bin| ["--bin".into(), bin.clone()])
            .chain(
                config
                    .example
                    .iter()
                    .flat_map(|name| ["--example".into(), name.clone()]),
            )
            .collect(),
        env: Vec::new(),
        build_root: None,
    });
    let mut args = vec!["build".into()];
    args.extend(shape.args);
    args.push("--message-format=json".into());

    Ok(args)
}

fn resolve_bin_name(metadata: &CargoMetadata, config: &BuildConfig) -> Result<String, DevError> {
    if let Some(ref bin) = config.bin {
        return Ok(bin.clone());
    }

    let package = if let Some(ref name) = config.package {
        metadata
            .packages
            .iter()
            .find(|pkg| pkg.name == *name)
            .ok_or_else(|| DevError::Build(format!("package '{name}' not found in metadata")))?
    } else {
        metadata
            .packages
            .first()
            .ok_or_else(|| DevError::Build("cargo metadata has no packages".into()))?
    };

    let bins: Vec<_> = package
        .targets
        .iter()
        .filter(|target| target.kind.iter().any(|kind| kind == "bin"))
        .collect();

    if bins.is_empty() {
        return Err(DevError::Build(
            "no binary targets found in package metadata".into(),
        ));
    }

    if bins.len() == 1 {
        return Ok(bins[0].name.clone());
    }

    if let Some(ref pkg) = config.package
        && let Some(named) = bins.iter().find(|target| target.name == *pkg)
    {
        return Ok(named.name.clone());
    }

    Ok(bins[0].name.clone())
}

/// Scan JSON lines from cargo output to find the compiled executable.
///
/// Prefer the executable whose file stem matches `expected_name`. When there
/// is no exact match, require cargo to have reported exactly one executable;
/// cargo does not guarantee JSON ordering, so selecting the last is unsafe.
/// The same single-executable rule applies when no name was supplied.
pub fn find_executable(stdout: &[u8], expected_name: Option<&str>) -> Result<PathBuf, DevError> {
    find_executable_artifact(stdout, expected_name).map(|artifact| artifact.path)
}

struct CargoExecutableArtifact {
    path: PathBuf,
    target: String,
}

/// Find the executable Cargo artifact and retain the target name emitted in
/// the same compiler-artifact message. Selection and ambiguity rules match
/// `find_executable` so callers still identify the exact binary they asked
/// Cargo to build.
fn find_executable_artifact(
    stdout: &[u8],
    expected_name: Option<&str>,
) -> Result<CargoExecutableArtifact, DevError> {
    let text = String::from_utf8_lossy(stdout);
    let mut all_exes: Vec<CargoExecutableArtifact> = Vec::new();
    let mut last_exe: Option<CargoExecutableArtifact> = None;
    let mut matched_exe: Option<CargoExecutableArtifact> = None;

    for (line_index, line) in text.lines().enumerate() {
        let line_number = line_index + 1;
        let val =
            crate::cargo_json::parse_cargo_json_line(line, line_number).map_err(DevError::Build)?;
        let Some(val) = val else {
            continue;
        };
        let Some(reason) = val.get("reason").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if reason != "compiler-artifact" {
            if val
                .get("executable")
                .is_some_and(|executable| !executable.is_null())
            {
                return Err(DevError::Build(format!(
                    "unexpected cargo JSON record `{reason}` with an executable on stdout line {line_number}"
                )));
            }
            continue;
        }
        let Some(executable) = val.get("executable") else {
            return Err(DevError::Build(format!(
                "malformed compiler-artifact on cargo stdout line {line_number}: missing `executable`"
            )));
        };
        if executable.is_null() {
            continue;
        }
        let Some(exe) = executable.as_str() else {
            return Err(DevError::Build(format!(
                "malformed compiler-artifact on cargo stdout line {line_number}: `executable` is not a string or null"
            )));
        };
        if exe.is_empty() {
            return Err(DevError::Build(format!(
                "malformed compiler-artifact on cargo stdout line {line_number}: empty `executable`"
            )));
        }
        let path = PathBuf::from(exe);
        let target = val
            .get("target")
            .and_then(|target| target.get("name"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
            .ok_or_else(|| {
                DevError::Build("cargo executable artifact missing target name".into())
            })?;
        let artifact = CargoExecutableArtifact { path, target };
        if expected_name.is_some()
            && artifact.path.file_stem().and_then(|s| s.to_str()) == expected_name
        {
            matched_exe = Some(CargoExecutableArtifact {
                path: artifact.path.clone(),
                target: artifact.target.clone(),
            });
        }
        all_exes.push(CargoExecutableArtifact {
            path: artifact.path.clone(),
            target: artifact.target.clone(),
        });
        last_exe = Some(artifact);
    }

    if let Some(exe) = matched_exe {
        return Ok(exe);
    }

    // No name to match - require exactly one executable to avoid
    // order-dependent behaviour (cargo doesn't guarantee JSON ordering).
    match last_exe {
        Some(exe) if all_exes.len() == 1 => Ok(exe),
        Some(_) => Err(DevError::Build(format!(
            "found {} executables in cargo output but no expected name to disambiguate; \
             specify --bin or -p",
            all_exes.len()
        ))),
        None => Err(DevError::Build("no executable in cargo output".into())),
    }
}

/// Extract compiler diagnostics from `--message-format=json` stdout and print
/// the rendered messages.  With JSON message format, cargo sends the actual
/// error details (file, line, message) as JSON to stdout - stderr only contains
/// the "Compiling…" progress lines and the final summary.
///
/// Warnings go through [`output::warn`] and everything else through
/// [`output::error`]: a failed build's lints are not the failure.
fn dump_compiler_messages(stdout: &[u8]) {
    for (is_warning, rendered) in rendered_diagnostics(stdout) {
        if is_warning {
            output::warn(&rendered);
        } else {
            output::error(&rendered);
        }
    }
}

/// The non-warning rendered diagnostics in cargo's JSON stdout, through
/// [`output::error`] alone, for a caller whose error output may be held back
/// (`output::capture_errors`) and replayed: warnings bypass that hold.
pub(crate) fn dump_compiler_errors(stdout: &[u8]) {
    for (_, rendered) in rendered_diagnostics(stdout).into_iter().filter(|(w, _)| !w) {
        output::error(&rendered);
    }
}

/// The rendered `compiler-message` diagnostics in cargo's JSON stdout, in
/// order, each paired with whether its level is `warning`.
fn rendered_diagnostics(stdout: &[u8]) -> Vec<(bool, String)> {
    let text = String::from_utf8_lossy(stdout);
    let mut out = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        let line_number = line_index + 1;
        let val = match crate::cargo_json::parse_cargo_json_line(line, line_number) {
            Ok(Some(val)) => val,
            Ok(None) => continue,
            Err(error) => {
                out.push((false, error));
                continue;
            }
        };
        // Not a cargo record, or one of the record kinds that carries no
        // diagnostic (cargo may add new kinds).
        if val.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(message) = val.get("message").and_then(serde_json::Value::as_object) else {
            out.push((
                false,
                format!(
                    "malformed compiler-message on cargo stdout line {line_number}: missing object `message`"
                ),
            ));
            continue;
        };
        let Some(level) = message.get("level").and_then(serde_json::Value::as_str) else {
            out.push((
                false,
                format!(
                    "malformed compiler-message on cargo stdout line {line_number}: missing string `message.level`"
                ),
            ));
            continue;
        };
        let Some(message_text) = message.get("message").and_then(serde_json::Value::as_str) else {
            out.push((
                false,
                format!(
                    "malformed compiler-message on cargo stdout line {line_number}: missing string `message.message`"
                ),
            ));
            continue;
        };
        let rendered = message
            .get("rendered")
            .and_then(serde_json::Value::as_str)
            .filter(|rendered| !rendered.trim().is_empty())
            .map(|rendered| rendered.trim_end().to_owned())
            .unwrap_or_else(|| format!("{level}: {message_text}"));
        out.push((level == "warning", rendered));
    }
    out
}

/// Print captured stderr through the error output channel, minus cargo's
/// progress chatter. This is the only place the cause appears when cargo dies
/// before it emits any JSON diagnostic (a lockfile or rustc-guard refusal, a
/// resolver error, a bad manifest), so it is printed whether or not
/// diagnostics were rendered.
fn dump_build_stderr(stderr: &[u8]) {
    let text = filtered_build_stderr(stderr);
    if !text.is_empty() {
        output::error(&text);
    }
}

/// `stderr` without the `Compiling`/`Fresh`/`Finished` progress lines.
fn filtered_build_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|l| {
            let first = l.split_whitespace().next();
            !matches!(first, Some("Compiling" | "Fresh" | "Finished"))
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_owned()
}

#[cfg(test)]
mod failure_report_tests {
    use super::{filtered_build_stderr, rendered_diagnostics};

    #[test]
    fn stderr_keeps_the_cause_and_drops_progress_lines() {
        let stderr =
            b"   Compiling foo v0.1.0\nerror: failed to parse manifest\n    Finished dev\n";
        assert_eq!(
            filtered_build_stderr(stderr),
            "error: failed to parse manifest"
        );
    }

    #[test]
    fn progress_only_stderr_filters_to_nothing() {
        assert_eq!(filtered_build_stderr(b"   Compiling foo v0.1.0\n"), "");
    }

    #[test]
    fn diagnostics_are_split_by_level() {
        let stdout = concat!(
            "{\"reason\":\"compiler-message\",\"message\":{\"level\":\"warning\",\"message\":\"w\",\"rendered\":\"warning: w\\n\"}}\n",
            "{\"reason\":\"compiler-message\",\"message\":{\"level\":\"error\",\"message\":\"e\",\"rendered\":\"error: e\\n\"}}\n",
            "{\"reason\":\"compiler-artifact\"}\n",
            "not json\n",
        );
        let d = rendered_diagnostics(stdout.as_bytes());
        assert_eq!(
            d,
            vec![
                (true, "warning: w".to_owned()),
                (false, "error: e".to_owned())
            ]
        );
    }
}
