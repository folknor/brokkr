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
    /// External Java/Maven build (Planetiler baseline).
    Java,
    /// External CMake/C++ build (Tilemaker baseline).
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

    /// Release build with features from owned strings (e.g. host config features).
    pub fn release_with_owned_features(package: Option<&str>, features: &[String]) -> Self {
        Self {
            package: package.map(std::borrow::ToOwned::to_owned),
            bin: None,
            example: None,
            features: features.to_vec(),
            default_features: true,
            all_features: false,
            profile: RustProfile::Release,
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
            profile: RustProfile::Release,
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
// Builds versus live executable handles
// ---------------------------------------------------------------------------

/// Process-wide exclusion between cargo runs and live
/// [`PbfhoggBinary`](crate::pbfhogg::binary::PbfhoggBinary) handles.
///
/// A handle probed its executable's capabilities and spawns that path; a
/// rebuild into the same target would replace the bytes under it. So a cargo
/// spawn through [`with_published_cargo`] is refused before it starts while
/// any handle is live, and a handle cannot register while a build runs.
/// Builds are exclusive too, which also keeps the single published cargo PID
/// truthful. Other processes are excluded by the brokkr lock, which a handle
/// holds for its whole life. Cargo started any other way (`cargo metadata`
/// through `output::run_cargo`, read-only and run before handles exist) is
/// outside this protocol.
pub(crate) mod executable_guard {
    use std::sync::{Mutex, PoisonError};

    use crate::error::DevError;

    struct State {
        live_handles: usize,
        building: bool,
    }

    /// The admission state, behind one mutex. Every check-and-change happens
    /// under it at once; nothing else - no filesystem work, no callback -
    /// runs while it is held.
    pub(crate) struct Registry {
        state: Mutex<State>,
    }

    impl Registry {
        pub(crate) const fn new() -> Self {
            Self {
                state: Mutex::new(State {
                    live_handles: 0,
                    building: false,
                }),
            }
        }

        fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
            f(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner))
        }

        pub(crate) fn begin_build(&'static self) -> Result<BuildToken, DevError> {
            self.with(|state| {
                if state.live_handles > 0 {
                    return Err(DevError::Internal(format!(
                        "refusing to run cargo while {} pbfhogg handle(s) are live - a \
                         build would replace an executable a handle is still using",
                        state.live_handles
                    )));
                }
                if state.building {
                    return Err(DevError::Internal(
                        "refusing to start a second concurrent cargo build".into(),
                    ));
                }
                state.building = true;
                Ok(BuildToken { registry: self })
            })
        }

        pub(crate) fn reserve_handle(&'static self) -> Result<HandleToken, DevError> {
            self.with(|state| {
                if state.building {
                    return Err(DevError::Internal(
                        "refusing to make a pbfhogg handle while a cargo build is running".into(),
                    ));
                }
                state.live_handles += 1;
                Ok(HandleToken { registry: self })
            })
        }
    }

    /// Held while one cargo process runs; dropping it, on return or unwind,
    /// ends the build.
    pub(crate) struct BuildToken {
        registry: &'static Registry,
    }

    impl Drop for BuildToken {
        fn drop(&mut self) {
            self.registry.with(|state| state.building = false);
        }
    }

    /// One live handle's registration; dropping it deregisters. Taken before
    /// the handle reads its executable, so no build can run between that read
    /// and the handle's last use.
    pub(crate) struct HandleToken {
        registry: &'static Registry,
    }

    impl Drop for HandleToken {
        fn drop(&mut self) {
            self.registry
                .with(|state| state.live_handles = state.live_handles.saturating_sub(1));
        }
    }

    #[cfg(not(test))]
    static PROCESS: Registry = Registry::new();

    // Unit tests run concurrently in one process, and one test's fixture
    // handle must not refuse another test's build, so under test each thread
    // gets its own registry. The registry itself is the production type;
    // its cross-thread exclusion is tested on a shared instance below.
    #[cfg(test)]
    thread_local! {
        static PROCESS: &'static Registry = Box::leak(Box::new(Registry::new()));
    }

    fn process() -> &'static Registry {
        #[cfg(not(test))]
        {
            &PROCESS
        }
        #[cfg(test)]
        {
            PROCESS.with(|registry| *registry)
        }
    }

    pub(crate) fn begin_build() -> Result<BuildToken, DevError> {
        process().begin_build()
    }

    pub(crate) fn reserve_handle() -> Result<HandleToken, DevError> {
        process().reserve_handle()
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used, clippy::expect_used)]
        use std::sync::{Arc, Barrier};

        use super::{Registry, begin_build, reserve_handle};

        fn shared() -> &'static Registry {
            Box::leak(Box::new(Registry::new()))
        }

        #[test]
        fn a_live_handle_refuses_a_build_until_it_drops() {
            let handle = reserve_handle().unwrap();
            assert!(begin_build().is_err());
            drop(handle);
            drop(begin_build().unwrap());
        }

        #[test]
        fn a_running_build_refuses_a_handle_and_a_second_build() {
            let build = begin_build().unwrap();
            assert!(reserve_handle().is_err());
            assert!(begin_build().is_err());
            drop(build);
            drop(reserve_handle().unwrap());
        }

        #[test]
        fn every_handle_must_drop_before_a_build() {
            let a = reserve_handle().unwrap();
            let b = reserve_handle().unwrap();
            drop(a);
            assert!(begin_build().is_err());
            drop(b);
            drop(begin_build().unwrap());
        }

        #[test]
        fn a_build_that_unwinds_still_releases() {
            let unwound = std::panic::catch_unwind(|| {
                let _build = begin_build().unwrap();
                panic!("cargo observer panicked");
            });
            assert!(unwound.is_err());
            drop(reserve_handle().unwrap());
        }

        #[test]
        fn exclusion_holds_across_threads() {
            // A handle held on one thread refuses a build on another, and a
            // build running on one thread refuses a handle on another.
            let registry = shared();
            let handle = registry.reserve_handle().unwrap();
            let refused = std::thread::spawn(move || registry.begin_build().is_err());
            assert!(refused.join().unwrap());
            drop(handle);

            let build = registry.begin_build().unwrap();
            let refused = std::thread::spawn(move || registry.reserve_handle().is_err());
            assert!(refused.join().unwrap());
            drop(build);
            drop(registry.reserve_handle().unwrap());
        }

        #[test]
        fn racing_threads_never_hold_a_build_beside_a_handle() {
            // Each round, half the threads race for handles and half for a
            // build, and every admitted token is held across a rendezvous.
            // While held, the shared counts must never show a handle and a
            // build at once, nor two builds.
            use std::sync::atomic::{AtomicUsize, Ordering};
            let registry = shared();
            let handles: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
            let builds: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
            let threads = 8;
            let barrier = Arc::new(Barrier::new(threads));
            let workers: Vec<_> = (0..threads)
                .map(|i| {
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        for round in 0..100 {
                            barrier.wait();
                            let (handle, build) = if (i + round) % 2 == 0 {
                                (registry.reserve_handle().ok(), None)
                            } else {
                                (None, registry.begin_build().ok())
                            };
                            if handle.is_some() {
                                handles.fetch_add(1, Ordering::SeqCst);
                            }
                            if build.is_some() {
                                builds.fetch_add(1, Ordering::SeqCst);
                            }
                            barrier.wait();
                            let (h, b) = (
                                handles.load(Ordering::SeqCst),
                                builds.load(Ordering::SeqCst),
                            );
                            assert!(b <= 1, "two builds admitted at once");
                            assert!(h == 0 || b == 0, "{h} handle(s) beside a build");
                            barrier.wait();
                            if handle.is_some() {
                                handles.fetch_sub(1, Ordering::SeqCst);
                            }
                            if build.is_some() {
                                builds.fetch_sub(1, Ordering::SeqCst);
                            }
                            drop((handle, build));
                        }
                    })
                })
                .collect();
            for worker in workers {
                worker.join().unwrap();
            }
            // Quiescent again: both kinds are admitted once everything dropped.
            drop(registry.reserve_handle().unwrap());
            drop(registry.begin_build().unwrap());
        }

        #[test]
        fn the_cargo_choke_point_refuses_while_a_handle_lives() {
            // The wiring, not just the registry: the build seam itself is
            // refused before cargo starts, and before it even asks for the lock.
            let _handle = reserve_handle().unwrap();
            let Err(err) = crate::build::cargo_build(
                &crate::build::BuildConfig::release(None),
                std::path::Path::new("."),
            ) else {
                panic!("a build under a live handle must be refused");
            };
            assert!(err.to_string().contains("pbfhogg handle"), "got: {err}");
        }
    }
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
    // Before cargo starts, and before anything else: a build may not run
    // while a pbfhogg handle exists, so no handle's executable is replaced
    // under it, whatever the caller does with the refusal.
    let _building = executable_guard::begin_build()?;
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

/// Body of [`cargo_build_observed`], run once the hold is established.
fn cargo_build_locked(
    config: &BuildConfig,
    project_root: &Path,
    on_spawn: &dyn Fn(u32),
    isolation: crate::shutdown::ChildIsolation<'_>,
) -> Result<BuiltArtifact, DevError> {
    let profile = config.profile.clone();
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
    // the example path fell through to `find_executable_artifact`'s "exactly one
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
        build_root: project_root.to_path_buf(),
        unit_profile: executable.unit_profile,
    };

    output::build_msg(&format!(
        "done in {} -> {}",
        output::format_duration(elapsed),
        built.path.display(),
    ));

    Ok(built)
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
            path.display()
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

pub(crate) struct CargoExecutableArtifact {
    pub(crate) path: PathBuf,
    target: String,
    /// The record's `profile` object, as [`crate::build_provenance::unit_pairs`].
    pub(crate) unit_profile: Vec<(String, String)>,
}

/// Scan cargo's JSON output for the compiled executable, keeping the target
/// name and resolved profile from the same compiler-artifact message.
///
/// Prefer the executable whose file stem matches `expected_name`. When there
/// is no exact match, require cargo to have reported exactly one executable;
/// cargo does not guarantee JSON ordering, so selecting the last is unsafe.
/// The same single-executable rule applies when no name was supplied.
pub(crate) fn find_executable_artifact(
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
        let unit_profile = val
            .get("profile")
            .map(crate::build_provenance::unit_pairs)
            .unwrap_or_default();
        let artifact = CargoExecutableArtifact {
            path,
            target,
            unit_profile,
        };
        if expected_name.is_some()
            && artifact.path.file_stem().and_then(|s| s.to_str()) == expected_name
        {
            matched_exe = Some(CargoExecutableArtifact {
                path: artifact.path.clone(),
                target: artifact.target.clone(),
                unit_profile: artifact.unit_profile.clone(),
            });
        }
        all_exes.push(CargoExecutableArtifact {
            path: artifact.path.clone(),
            target: artifact.target.clone(),
            unit_profile: artifact.unit_profile.clone(),
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
