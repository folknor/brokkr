use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::cargo::metadata::{self, CargoMetadata, Query};
use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Profiles and build kinds
// ---------------------------------------------------------------------------

/// The prefix that marks a custom profile in a stored build-kind column.
/// Unprefixed values are the legacy closed set, so a custom profile named
/// `java` can never be read back as the Java baseline.
const STORED_NAMED_PREFIX: &str = "cargo:";

/// A custom Cargo profile name (`[profile.<name>]`), syntax-checked.
///
/// Two constructors, because selecting and decoding are different questions:
/// [`Self::select`] also refuses the names brokkr or Cargo own at the target
/// root ([`crate::target_namespace`]), while [`Self::historical`] only checks
/// syntax, so a reservation added later never makes a stored row unreadable.
/// Whether the profile exists is Cargo's to answer, at build time.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProfileName(String);

impl ProfileName {
    /// A name a build may select.
    pub fn select(name: &str) -> Result<Self, String> {
        let name = Self::historical(name)?;
        match crate::target_namespace::reserved_reason(&name.0) {
            Some(why) => Err(format!("cargo profile `{}` cannot be used: {why}", name.0)),
            None => Ok(name),
        }
    }

    /// A name read back from a store: syntax only.
    fn historical(name: &str) -> Result<Self, String> {
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if valid {
            Ok(Self(name.to_owned()))
        } else {
            Err(format!(
                "`{name}` is not a cargo profile name (ASCII letters, digits, `-` and `_` only)"
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The Cargo profile a Rust build runs under.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum RustProfile {
    /// `cargo build --release` - the standard brokkr-built binary.
    #[default]
    Release,
    /// Cargo's dev profile, which builds into `<target>/debug/`.
    Dev,
    /// A custom `[profile.<name>]` - `cargo build --profile <name>`, built
    /// into `<target>/<name>/`.
    Named(ProfileName),
}

impl RustProfile {
    /// The profile a `debug` flag selects.
    pub fn for_debug(debug: bool) -> Self {
        if debug { Self::Dev } else { Self::Release }
    }

    /// Parse a selection from config or the CLI: `dev` and `release` are the
    /// built-ins, any other name a custom profile. `debug` is refused rather
    /// than aliased - it is a directory, not a profile.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "dev" => Ok(Self::Dev),
            "release" => Ok(Self::Release),
            other => ProfileName::select(other).map(Self::Named),
        }
    }

    /// The profile's name as Cargo spells it.
    pub fn label(&self) -> &str {
        match self {
            Self::Release => "release",
            Self::Dev => "dev",
            Self::Named(name) => name.as_str(),
        }
    }

    /// Cargo's directory for this profile's artifacts under the target root.
    pub fn target_dir(&self) -> &str {
        match self {
            Self::Release => "release",
            Self::Dev => "debug",
            Self::Named(name) => name.as_str(),
        }
    }

    pub fn is_dev(&self) -> bool {
        matches!(self, Self::Dev)
    }
}

impl std::fmt::Display for RustProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl<'de> serde::Deserialize<'de> for RustProfile {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// A config table's profile default from its `profile` and legacy `debug`
/// keys. `profile` wins when both are present - composition can leave both
/// set from different layers, and `profile` is the only key that can name
/// every profile, so it is the one a higher layer can always override with.
/// The warning describes the config's resolution, not the build: a CLI flag
/// may still override the result.
pub fn configured_profile(
    table: &str,
    profile: Option<&RustProfile>,
    debug: Option<bool>,
) -> Option<RustProfile> {
    match (profile, debug) {
        (Some(profile), Some(debug)) => {
            output::warn(&format!(
                "{table}: config profile = \"{profile}\" overrides config debug = {debug}; \
                 debug is ignored"
            ));
            Some(profile.clone())
        }
        (Some(profile), None) => Some(profile.clone()),
        (None, debug) => debug.map(RustProfile::for_debug),
    }
}

/// Identifies how a measured binary was built. Stored on every result row
/// (column `cargo_profile`) so Rust-built runs stay distinguishable from
/// each other and from external baselines that go through entirely
/// different build systems.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BuildKind {
    /// A Cargo build under the given profile.
    Cargo(RustProfile),
    /// External Java/Maven build: the retired Planetiler baseline's rows.
    Java,
    /// External CMake/C++ build: the retired Tilemaker baseline's rows.
    CMake,
}

impl Default for BuildKind {
    fn default() -> Self {
        Self::Cargo(RustProfile::Release)
    }
}

impl BuildKind {
    /// The human-facing label: a custom profile renders as its bare name.
    /// Never written to a store - see [`Self::to_stored`].
    pub fn as_label(&self) -> &str {
        match self {
            Self::Cargo(profile) => profile.label(),
            Self::Java => "java",
            Self::CMake => "cmake",
        }
    }

    /// The value written to a store's profile column. Built-ins keep their
    /// legacy spelling; a custom profile is `cargo:<name>`.
    pub fn to_stored(&self) -> String {
        match self {
            Self::Cargo(RustProfile::Named(name)) => {
                format!("{STORED_NAMED_PREFIX}{}", name.as_str())
            }
            other => other.as_label().to_owned(),
        }
    }

    /// Parse a stored value. Unprefixed values are the closed legacy set, so
    /// a typo still surfaces as `Err`; `cargo:<name>` is syntax-checked only
    /// (today's reservations do not reach back into history), and the
    /// non-canonical `cargo:dev`/`cargo:release` are refused.
    pub fn from_db(s: &str) -> Result<Self, UnknownCargoProfile> {
        let unknown = || UnknownCargoProfile(s.to_owned());
        match s {
            "release" => Ok(Self::Cargo(RustProfile::Release)),
            "dev" => Ok(Self::Cargo(RustProfile::Dev)),
            "java" => Ok(Self::Java),
            "cmake" => Ok(Self::CMake),
            other => {
                let name = other
                    .strip_prefix(STORED_NAMED_PREFIX)
                    .ok_or_else(unknown)?;
                if matches!(name, "dev" | "release") {
                    return Err(unknown());
                }
                ProfileName::historical(name)
                    .map(|n| Self::Cargo(RustProfile::Named(n)))
                    .map_err(|_| unknown())
            }
        }
    }
}

#[derive(Debug)]
pub struct UnknownCargoProfile(pub String);

/// The executable Cargo actually produced for a build request, together with
/// the build settings needed to describe that artifact on a measurement row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltArtifact {
    pub path: PathBuf,
    pub profile: RustProfile,
    /// Explicit feature requests passed to this Cargo invocation.
    pub features: Vec<String>,
    /// Cargo target name reported with the executable artifact.
    pub target: String,
    /// The directory cargo ran in: what decides the config chain the build
    /// read, and so its host-side provenance (`crate::build_provenance`).
    pub build_root: PathBuf,
    /// Cargo's resolved profile for this executable, from its
    /// compiler-artifact record ([`crate::build_provenance::unit_pairs`]).
    pub unit_profile: Vec<(String, String)>,
    /// The features cargo resolved for the executable's unit, from the same
    /// record, in cargo's order.
    pub unit_features: Vec<String>,
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
    /// `--all-features`, set from the `--all-features` of `brokkr run` and
    /// `brokkr build` (and `run --with-all-features`).
    pub all_features: bool,
    pub profile: RustProfile,
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
            profile: RustProfile::Release,
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
            profile: RustProfile::Release,
        }
    }

    /// Build config for an external harness crate described by a `[*.harness]`
    /// block (`package` + optional `binary`/`features`), under an
    /// already-resolved profile. Shared by every harness build
    /// (`harness_build`): they build the same kind of engine, so the config
    /// mapping lives once here.
    pub fn for_harness_with_profile(
        harness_cfg: &crate::config::HarnessConfig,
        profile: RustProfile,
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
    // A build is minutes of captured cargo whose supervisor polls the shutdown
    // flag. Without a guard in scope a `brokkr kill` takes the default action,
    // killing brokkr and orphaning cargo in its process group. Guards nest, so
    // callers that already hold one are unaffected.
    let _interrupts = crate::shutdown::SigtermGuard::install();
    let _activity = crate::lockfile::activity::Activity::begin("cargo build");
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

/// How a script bench's `rustflags` reach one cargo build: injected at the
/// live rustflags layer, never displacing the project's own flags, and built
/// in a target dir of the recipe's own, intermediates included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustflagsInjection {
    pub placement: Injected,
    /// `<base>/rustflags-<hash>`: `base` the worktree's isolated target for a
    /// `--commit` build, else cargo's target directory.
    pub target_dir: PathBuf,
}

/// What [`RustflagsInjection`] adds to the cargo invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Injected {
    /// An empty recipe: nothing added.
    None,
    /// The live env source with the additions appended: the whole value.
    Env { key: String, value: String },
    /// `--config` arguments at the live config layer.
    Config { args: Vec<String> },
}

impl RustflagsInjection {
    /// Where `additions` go for a build rooted at `build_root`. Refused when
    /// the live layer cannot be proven: the fallback could leave the flags
    /// inert, and a run labelled with flags that never reached rustc is
    /// worse than no run.
    pub fn place(build_root: &Path, additions: &[String]) -> Result<Self, DevError> {
        let placement = if additions.is_empty() {
            Injected::None
        } else {
            let placed = crate::rustflags::place(
                crate::rustflags::FlagKind::Rustc,
                build_root,
                None,
                crate::rustflags::EffectiveEnv::new(&[]),
                additions,
            )?;
            if !placed.certain {
                return Err(DevError::Refused(
                    "the bench's rustflags cannot be placed with certainty: the cargo config \
                     chain holds a target rustflags table brokkr cannot judge (an unmodelled \
                     cfg, configured triples that disagree, or an unreadable file), so the \
                     flags might never reach rustc"
                        .into(),
                ));
            }
            match placed.env {
                Some((key, value)) => Injected::Env { key, value },
                None => Injected::Config { args: placed.args },
            }
        };
        let base = match cargo_target_dir_override(build_root) {
            Some(dir) => PathBuf::from(dir),
            None => project_info(Some(build_root))?.target_dir,
        };
        Ok(Self {
            placement,
            target_dir: base.join(format!(
                "{}{}",
                crate::target_namespace::RUSTFLAGS_PREFIX,
                recipe_key(additions)
            )),
        })
    }
}

/// The target dir key of a bench's resolved `rustflags`: FNV-1a over a
/// domain tag and each token length-prefixed, so no two token vectors share
/// a key by concatenation, and a `[[check]]` sweep of the same flags (which
/// exports them as `RUSTFLAGS`, a different build) never shares the dir.
fn recipe_key(additions: &[String]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(b"brokkr bench rustflags\0");
    for token in additions {
        eat(&(token.len() as u64).to_le_bytes());
        eat(token.as_bytes());
    }
    format!("{hash:016x}")
}

/// [`cargo_build`] with a bench's `rustflags` injected and its target dir
/// isolated.
pub fn cargo_build_with_rustflags(
    config: &BuildConfig,
    project_root: &Path,
    injection: &RustflagsInjection,
) -> Result<BuiltArtifact, DevError> {
    with_published_cargo(None, |publish| {
        cargo_build_inner(
            config,
            project_root,
            publish,
            crate::shutdown::ChildIsolation::Inherit,
            Some(injection),
        )
    })
}

/// Body of [`cargo_build_observed`], run once the hold is established.
fn cargo_build_locked(
    config: &BuildConfig,
    project_root: &Path,
    on_spawn: &dyn Fn(u32),
    isolation: crate::shutdown::ChildIsolation<'_>,
) -> Result<BuiltArtifact, DevError> {
    cargo_build_inner(config, project_root, on_spawn, isolation, None)
}

fn cargo_build_inner(
    config: &BuildConfig,
    project_root: &Path,
    on_spawn: &dyn Fn(u32),
    isolation: crate::shutdown::ChildIsolation<'_>,
    injection: Option<&RustflagsInjection>,
) -> Result<BuiltArtifact, DevError> {
    let profile = config.profile.clone();
    let on_spawn = Some(on_spawn);
    let mut args = build_args(config)?;
    if let Some(Injected::Config { args: extra }) = injection.map(|i| &i.placement) {
        // `--config` belongs after the subcommand, as any cargo option.
        args.splice(1..1, extra.iter().cloned());
    }
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
    let mut owned: Vec<(String, String)> = Vec::new();
    match injection {
        Some(injection) => {
            // The recipe's own dir, intermediates too: an inherited absolute
            // `build.build-dir` would otherwise share fingerprints across
            // checkouts and recipes.
            let dir = injection.target_dir.display().to_string();
            output::detail_or_console(&format!("bench rustflags build target dir -> {dir}"));
            owned.push(("CARGO_TARGET_DIR".into(), dir.clone()));
            owned.push(("CARGO_BUILD_BUILD_DIR".into(), dir));
            if let Injected::Env { key, value } = &injection.placement {
                owned.push((key.clone(), value.clone()));
            }
        }
        None => {
            if let Some(dir) = &target_override {
                output::detail_or_console(&format!("isolating worktree build target dir -> {dir}"));
                owned.push(("CARGO_TARGET_DIR".into(), dir.clone()));
            }
        }
    }
    let env: Vec<(&str, &str)> = owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

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

    let executable = crate::executable_artifact::find(
        &captured.stdout,
        &crate::executable_artifact::Selector {
            package: config.package.as_deref(),
            bin: config.bin.as_deref(),
            example: config.example.as_deref(),
        },
    )?;
    let built = BuiltArtifact {
        path: executable.path,
        profile,
        features: config.features.clone(),
        target: executable.target,
        build_root: project_root.to_path_buf(),
        unit_profile: executable.unit_profile,
        unit_features: executable.features,
    };

    output::build_msg(&format!(
        "done in {} -> {}",
        output::format_duration(elapsed),
        crate::output::path(&built.path),
    ));

    Ok(built)
}

/// What [`cargo_captured`] does with a successful build's compiler warnings.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompilerWarnings {
    /// Print them: nothing after this build shows them (`install`).
    Print,
    /// Leave them to the quiet `cargo run` that follows, which replays a
    /// fresh local crate's warnings itself, so each prints once (`run`).
    LeaveToRun,
}

/// `cargo` with `args` (built `--color=never --message-format=json`) in
/// `project_root`, its output captured: `run`'s prebuild and `install`.
///
/// On failure everything cargo said is printed, once, and the error is the
/// bare label `failure`. On success build-script `cargo::warning` lines and
/// cargo's own warnings are printed, and compiler warnings as `compiler`
/// says.
pub(crate) fn cargo_captured(
    args: &[String],
    env: &[(String, String)],
    project_root: &Path,
    failure: &str,
    compiler: CompilerWarnings,
) -> Result<(), DevError> {
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    output::build_msg(&format!("cargo {}", arg_refs.join(" ")));
    with_published_cargo(None, |publish| {
        let start = Instant::now();
        let captured = output::run_captured_observed(
            "cargo",
            &arg_refs,
            project_root,
            &env_refs,
            Some(publish),
            crate::shutdown::ChildIsolation::Inherit,
            output::SpawnProfile::Cargo { phase: None },
        )?;
        if !captured.success() {
            dump_compiler_messages(&captured.stdout);
            dump_build_stderr(&captured.stderr);
            return Err(DevError::Reported(failure.into()));
        }
        if compiler == CompilerWarnings::Print {
            dump_compiler_messages(&captured.stdout);
        }
        let warnings = prebuild_warnings(&captured.stderr);
        if !warnings.is_empty() {
            output::warn(&warnings);
        }
        output::build_msg(&format!(
            "done in {}",
            output::format_duration(start.elapsed())
        ));
        Ok(())
    })
}

/// The warning records of a successful build's stderr (built `--color=never`):
/// each `warning:` line with the lines that continue it, up to the next status
/// line or record. cargo's per-crate compiler-warning tallies are dropped; the
/// rendered compiler warnings themselves are on stdout as JSON, not here.
/// Anything before the first record (none is expected) is dropped too.
fn prebuild_warnings(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let mut kept: Vec<&str> = Vec::new();
    let mut keeping = false;
    for raw in text.lines() {
        // A forced progress bar (`term.progress.when = "always"`) redraws
        // with carriage returns; only what the last one left is the line.
        let line = raw.rsplit('\r').next().unwrap_or(raw);
        if line.trim().is_empty() && raw.contains('\r') {
            continue;
        }
        if is_progress_line(line)
            || line.trim_start().starts_with("Building [")
            || line.starts_with("error:")
        {
            keeping = false;
        } else if line.starts_with("warning:") {
            keeping = !is_warning_summary(line);
            if keeping {
                kept.push(line);
            }
        } else if keeping {
            kept.push(line);
        }
    }
    kept.join("\n").trim().to_owned()
}

/// A cargo status line: a verb right-aligned to cargo's 12 columns, a space,
/// then what it acted on. The alignment is what tells it from a warning's
/// continuation line that happens to start with the same word.
fn is_progress_line(line: &str) -> bool {
    let Some((head, _)) = line.split_at_checked(12) else {
        return false;
    };
    if !line[12..].starts_with(' ') {
        return false;
    }
    matches!(
        Some(head.trim_start()),
        Some(
            "Compiling"
                | "Fresh"
                | "Finished"
                | "Checking"
                | "Building"
                | "Blocking"
                | "Downloading"
                | "Downloaded"
                | "Locking"
                | "Updating"
                | "Adding"
                | "Unpacking"
                | "Running"
                | "Documenting"
                | "Installing"
                | "Installed"
                | "Replacing"
                | "Replaced"
                | "Ignored"
                | "Summary"
        )
    )
}

/// cargo's `warning: `pkg` (bin "x") generated 3 warnings` tally, which
/// counts compiler warnings the run replays with their own tally.
fn is_warning_summary(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("warning: `") else {
        return false;
    };
    let Some((_, tally)) = rest.split_once(" generated ") else {
        return false;
    };
    let digits = tally.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    // ` warning` or ` warnings`, then the end or a parenthesised note
    // (`(run `cargo fix ...`)`, `(1 duplicate)`).
    let after = &tally[digits..];
    let Some(rest) = after
        .strip_prefix(" warnings")
        .or_else(|| after.strip_prefix(" warning"))
    else {
        return false;
    };
    rest.is_empty() || rest.starts_with(" (")
}

/// Resolve the selected profile's binary path from cargo metadata without
/// building. A lookup only: nothing here checks that the binary found was
/// built with the requested features, or under the profile's current
/// definition, and a custom profile's existence goes unchecked because
/// Cargo, which validates it, never runs.
pub fn resolve_existing_binary(
    config: &BuildConfig,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let target_override = cargo_target_dir_override(project_root);
    let env = target_dir_env(target_override.as_deref());
    let metadata = metadata::load(project_root, Query::workspace(), &env, None)?;
    let bin_name = resolve_bin_name(&metadata, config)?;

    let mut path = PathBuf::from(&metadata.target_directory);
    path.push(config.profile.target_dir());
    path.push(bin_name);

    if !path.exists() {
        return Err(DevError::Build(format!(
            "binary not found at {} (build first or omit --no-build)",
            crate::output::path(&path)
        )));
    }

    Ok(path)
}

/// Assemble the argument list for `cargo build`.
fn build_args(config: &BuildConfig) -> Result<Vec<String>, DevError> {
    let purpose = crate::build_purpose::Purpose::Build;
    let profile = crate::build_purpose::profile(
        purpose,
        crate::build_purpose::Profiles {
            invocation: Some(config.profile.clone()),
            ..Default::default()
        },
    )?;
    let shape = crate::build_purpose::resolve(crate::build_purpose::Request {
        purpose,
        profile,
        packages: config
            .package
            .as_ref()
            .map_or_else(Vec::new, |pkg| vec!["-p".into(), pkg.clone()]),
        features: crate::build_purpose::Features::values(
            &config.features,
            config.all_features,
            !config.default_features,
            crate::build_purpose::FeatureRendering::CommaJoined,
        ),
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
mod profile_tests {
    #![allow(clippy::unwrap_used)]

    use super::{BuildKind, RustProfile};

    #[test]
    fn selection_parses_builtins_first_and_refuses_reserved_names() {
        assert_eq!(RustProfile::parse("dev").unwrap(), RustProfile::Dev);
        assert_eq!(RustProfile::parse("release").unwrap(), RustProfile::Release);
        let corpus = RustProfile::parse("corpus").unwrap();
        assert_eq!((corpus.label(), corpus.target_dir()), ("corpus", "corpus"));
        assert_eq!(RustProfile::Dev.target_dir(), "debug");
        for bad in [
            "debug",
            "test",
            "tmp",
            "unify-package",
            "fixture-x",
            "",
            "a b",
            "cargo:x",
        ] {
            assert!(RustProfile::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn stored_kinds_round_trip_without_colliding() {
        let java_named = BuildKind::Cargo(RustProfile::parse("java").unwrap());
        for kind in [
            BuildKind::default(),
            BuildKind::Cargo(RustProfile::Dev),
            BuildKind::Cargo(RustProfile::parse("corpus").unwrap()),
            java_named.clone(),
            BuildKind::Java,
            BuildKind::CMake,
        ] {
            assert_eq!(BuildKind::from_db(&kind.to_stored()).unwrap(), kind);
        }
        // A custom profile named `java` is never read back as the baseline.
        assert_eq!(java_named.to_stored(), "cargo:java");
        assert_ne!(BuildKind::from_db("java").unwrap(), java_named);
    }

    #[test]
    fn decoding_stays_strict_for_legacy_values_and_lenient_for_history() {
        for bad in [
            "relesae",
            "debug",
            "cargo:dev",
            "cargo:release",
            "cargo:",
            "cargo:a b",
        ] {
            assert!(BuildKind::from_db(bad).is_err(), "{bad:?}");
        }
        // A name reserved today still decodes from a row written before.
        assert!(BuildKind::from_db("cargo:tmp").is_ok());
    }
}

#[cfg(test)]
mod failure_report_tests {
    use super::{filtered_build_stderr, prebuild_warnings, rendered_diagnostics};

    #[test]
    fn a_successful_prebuild_keeps_script_and_cargo_warnings_only() {
        let stderr = concat!(
            "    Updating crates.io index\n",
            "   Compiling sys v0.1.0\n",
            "warning: sys@0.1.0: system library missing, using bundled\n",
            "warning: `app` (bin \"app\") generated 2 warnings\n",
            "warning: Patch `x v1.0.0` was not used in the crate graph.\n",
            "Running `cargo update` may fix it.\n",
            "    Finished `release` profile [optimized] target(s) in 1.2s\n",
        );
        assert_eq!(
            prebuild_warnings(stderr.as_bytes()),
            "warning: sys@0.1.0: system library missing, using bundled\n\
             warning: Patch `x v1.0.0` was not used in the crate graph.\n\
             Running `cargo update` may fix it."
        );
        assert_eq!(
            prebuild_warnings(b"   Compiling a v0.1.0\n    Finished dev\n"),
            ""
        );
        assert_eq!(
            prebuild_warnings(b"warning: `foo` generated a warning file\n"),
            "warning: `foo` generated a warning file"
        );
        assert_eq!(
            prebuild_warnings(b"    Building [==>  ] 3/9: a\r                  \rwarning: w\n"),
            "warning: w"
        );
        assert_eq!(prebuild_warnings(b"    Building [==>  ] 3/9: a\r\n"), "");
        assert_eq!(
            prebuild_warnings(b"warning: keep\nerror: drop\n"),
            "warning: keep"
        );
        assert_eq!(
            prebuild_warnings(b"warning: `foo` generated 2 warning files\n"),
            "warning: `foo` generated 2 warning files"
        );
        assert_eq!(
            prebuild_warnings(
                b"warning: `a` (lib) generated 1 warning (run `cargo fix --lib -p a` to apply 1 suggestion)\n"
            ),
            ""
        );
    }

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
