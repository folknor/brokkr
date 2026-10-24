//! Unified measurement types for the CLI redesign.
//!
//! Replaces the separate `BenchRequest` / `HotpathRequest` / `ProfileRequest`
//! structs with a single `MeasureRequest` carrying a `MeasureMode`.

use crate::build::BuildKind;
use std::path::Path;
use std::time::Duration;

use self::execution::{Iteration, Measurement};
use self::record::format_storage_notes;
use self::wall::{
    WallSource, bench_ordering_key, elapsed_to_ms, elapsed_to_us, summarize_distribution,
};
use crate::config;
use crate::db::{Distribution, HotpathData, KvPair, ResultsDb};
use crate::error::DevError;
use crate::git::GitInfo;
use crate::lockfile::LockGuard;
use crate::output::console;
use crate::project::{BuildRoot, Project, ProjectRoot};
use std::num::NonZeroUsize;

// ---------------------------------------------------------------------------
// MeasureMode
// ---------------------------------------------------------------------------

/// How to measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasureMode {
    /// Run once, print timing. Acquires lockfile but no DB storage. Default.
    Run,
    /// Full benchmark: lockfile, N runs (best-of-N), DB storage.
    Bench { runs: NonZeroUsize },
    /// Function-level timing via `--features hotpath` + `BenchHarness::capture_hotpath()`.
    Hotpath { runs: NonZeroUsize },
    /// Per-function allocation tracking via `--features hotpath-alloc` +
    /// `BenchHarness::capture_hotpath()`.
    Alloc { runs: NonZeroUsize },
}

/// A measurement mode that is stored on a results row.
///
/// Run mode has no database row, so it deliberately has no `RecordedMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedMode {
    Bench,
    Hotpath,
    Alloc,
}

impl RecordedMode {
    /// Stable label stored in the results database.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bench => "bench",
            Self::Hotpath => "hotpath",
            Self::Alloc => "alloc",
        }
    }
}

/// The profiling build selected by a measurement request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfiledMode {
    Hotpath,
    Alloc,
}

impl ProfiledMode {
    /// Cargo feature that enables this instrumentation.
    pub const fn feature(self) -> &'static str {
        match self {
            Self::Hotpath => "hotpath",
            Self::Alloc => "hotpath-alloc",
        }
    }

    /// Optional caveat printed before an allocation-profiled run.
    pub const fn note(self) -> Option<&'static str> {
        match self {
            Self::Hotpath => None,
            Self::Alloc => Some("NOTE: alloc profiling - wall-clock times are not meaningful"),
        }
    }

    /// Print [`Self::note`], when there is one. Every profiled entry point
    /// calls this, so the caveat's wording lives in one place.
    pub fn print_note(self) {
        if let Some(note) = self.note() {
            crate::output::hotpath_msg(note);
        }
    }
}

impl MeasureRequest<'_> {
    /// The cargo features a build for this request carries: the profiled
    /// mode's instrumentation feature first (when there is one), then the
    /// user/host features.
    pub fn build_features(&self) -> Vec<&str> {
        let mut all: Vec<&str> = self
            .mode
            .profiled()
            .map(ProfiledMode::feature)
            .into_iter()
            .collect();
        for feature in self.features.iter().map(String::as_str) {
            if !all.contains(&feature) {
                all.push(feature);
            }
        }
        all
    }
}

impl MeasureMode {
    /// Number of runs for this mode. Zero cannot be represented.
    pub const fn runs(self) -> NonZeroUsize {
        match self {
            Self::Run => NonZeroUsize::MIN,
            Self::Bench { runs } | Self::Hotpath { runs } | Self::Alloc { runs } => runs,
        }
    }

    /// The mode to persist, if this request records results.
    pub const fn recorded(self) -> Option<RecordedMode> {
        match self {
            Self::Run => None,
            Self::Bench { .. } => Some(RecordedMode::Bench),
            Self::Hotpath { .. } => Some(RecordedMode::Hotpath),
            Self::Alloc { .. } => Some(RecordedMode::Alloc),
        }
    }

    /// Profiling instrumentation selected by this mode, if any.
    pub const fn profiled(self) -> Option<ProfiledMode> {
        match self {
            Self::Hotpath { .. } => Some(ProfiledMode::Hotpath),
            Self::Alloc { .. } => Some(ProfiledMode::Alloc),
            Self::Run | Self::Bench { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// MeasureRequest
// ---------------------------------------------------------------------------

/// Unified request struct replacing `BenchRequest` / `HotpathRequest` /
/// `ProfileRequest`.
///
/// Carries everything needed to set up a measurement: project context, dataset
/// selection, build options, and the measurement mode.
pub struct MeasureRequest<'a> {
    /// Parsed `brokkr.toml` configuration.
    pub dev_config: &'a config::DevConfig,
    /// The section parsed for `project` (dellingr's workloads, mogwai's
    /// targets, piners' paths).
    pub project_config: &'a config::project_config::ProjectConfig,
    /// Which project we are running in.
    pub project: Project,
    /// Root of the project (where `brokkr.toml` lives): state, stores, data.
    pub project_root: &'a ProjectRoot,
    /// The code tree being built: the live checkout, or the `--commit`
    /// worktree cut from it.
    pub build_root: &'a BuildRoot,
    /// Dataset name from `--dataset` (e.g. `"denmark"`).
    pub dataset: &'a str,
    /// PBF variant from `--variant` (e.g. `"indexed"`, `"raw"`).
    pub variant: &'a str,
    /// Extra cargo features from CLI `--features` and host config.
    pub features: &'a [String],
    /// Allow benchmarking on a dirty git tree.
    pub force: bool,
    /// How to measure.
    pub mode: MeasureMode,
    /// The brokkr invocation: the exact argv and its arm key
    /// (`crate::run_invocation`). Stored on each result row so queries can
    /// grep the command the user actually typed, even when brokkr translates
    /// it into something different before handing off to the tool
    /// subprocess, and so `--compare` pairs rows without re-parsing it.
    pub invocation: &'a crate::db::RunInvocation,
    /// Dry-run: resolve paths and build the arg vector without building the
    /// binary or running it. See `ModeArgs::dry_run`.
    pub dry_run: bool,
    /// Kill the child when this marker is emitted via the sidecar FIFO.
    pub stop_marker: Option<&'a str>,
}

impl MeasureRequest<'_> {
    /// Number of measurement runs for this request.
    pub fn runs(&self) -> usize {
        self.mode.runs().get()
    }

    /// Feature refs as `&str` slices (for passing to build functions).
    pub fn feat_refs(&self) -> Vec<&str> {
        self.features.iter().map(String::as_str).collect()
    }
}

/// Shared execution of standalone command plans.
pub(crate) mod runner;

// ---------------------------------------------------------------------------
// Configuration and result types
// ---------------------------------------------------------------------------

/// Configuration for a benchmark run.
pub struct BenchConfig {
    pub command: String,
    pub input_file: Option<String>,
    pub input_mb: Option<f64>,
    /// Optional per-row feature label. When present, it takes precedence over
    /// the harness-wide build feature label; the DB writer stores a sorted,
    /// duplicate-free spelling so equivalent feature sets compare alike.
    pub cargo_features: Option<String>,
    /// The build identity of a row whose binary brokkr did not build with
    /// cargo (planetiler's Java, tilemaker's CMake). `None` for every cargo
    /// build: those rows take the profile the session's build actually used
    /// ([`BenchHarness::with_build_identity`]).
    pub external_build: Option<BuildKind>,
    pub runs: usize,
    /// Literal subprocess invocation (pbfhogg/elivagar/...). The
    /// invocation-taking external runners overwrite it with the rendering of
    /// the invocation they execute; other paths take it from dispatch.
    pub cli_args: Option<String>,
    pub metadata: Vec<KvPair>,
}

/// Result of a single benchmark measurement.
pub struct BenchResult {
    pub elapsed_ms: i64,
    /// Exact wall in microseconds, when the harness path knows it.
    ///
    /// `elapsed_ms` is an integer and always will be - every query, format
    /// and historical row depends on it. But sluggrs' measured regions are
    /// single-digit milliseconds and the deltas worth seeing there are
    /// 100-200us, which integer milliseconds flatten to nothing. Filled by
    /// the stderr-kv path (from a fractional `elapsed_ms=6.847` line) and by
    /// the paths brokkr times itself from an exact `Duration`
    /// (`run_external`/`run_external_ok`, `BenchHarness::capture_hotpath`, and
    /// `run_distribution`, whose closures hand back a `Duration` and which
    /// records the fastest sample here). `run_internal` carries whatever its
    /// closure returned; a closure that only had milliseconds leaves it
    /// `None`, and `elapsed_ms` remains the only timing available.
    ///
    /// Where both exist, `elapsed_ms` is this value rounded - never treat
    /// them as independent measurements.
    pub elapsed_us: Option<i64>,
    pub kv: Vec<KvPair>,
    /// Per-iteration wall times in execution order, attached by `record`
    /// to the aggregate best-of-N result.
    ///
    /// Always empty on the `BenchResult` a per-iteration closure hands back -
    /// one iteration doesn't have a list of iterations. Recording collects each
    /// iteration's `elapsed_ms` and attaches the vector to the winner, so
    /// `elapsed_ms` is the best of `iterations`.
    ///
    /// Ordering is the payload: `--bench N` reports the minimum, which hides
    /// whether the run drifted (iteration 3 slowest) or started cold
    /// (iteration 1 slowest). Never sort this.
    pub iterations: Vec<i64>,
    pub distribution: Option<Distribution>,
    pub hotpath: Option<HotpathData>,
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The benchmark harness. Holds lockfile guard, database, env snapshot, git info.
pub struct BenchHarness {
    lock: LockGuard,
    db: ResultsDb,
    /// The `.brokkr` stores the rows land in: the main tree's, also under
    /// `--commit` (`db_root`).
    state: crate::clean::layout::ProjectState,
    /// The host facts a row records, read once from the typed host probes
    /// (`crate::host`) - not the whole `brokkr env` report, whose tool and
    /// dataset probes a measurement never reads.
    host: HostFacts,
    git: GitInfo,
    storage_notes: Option<String>,
    cargo_features: Option<String>,
    project: crate::project::Project,
    stop_marker: Option<String>,
    /// The brokkr invocation (exact argv and arm key, `crate::run_invocation`).
    /// Set once per harness via `with_invocation`; every row built by this
    /// harness inherits it via `build_row`. Lets individual bench writers stay
    /// agnostic of the brokkr-level invocation.
    invocation: crate::db::RunInvocation,
    /// The recorded measurement mode. Set once per harness via
    /// `with_measure_mode`. The single source of a run's mode; read only
    /// through [`Self::effective_mode`].
    measure_mode: crate::measure::RecordedMode,
    /// Whether measurements from this harness are persisted - a switch apart
    /// from the mode. A request in run mode calls `with_measure_mode(None)`:
    /// it still reports the timing, but writes neither results nor sidecar
    /// rows.
    store_results: bool,
    /// What the session built, when it built the measured binary: the row's
    /// `cargo_profile`, canonical `cargo_features` and `provenance.*` pairs
    /// come from it. A cell, because a suite measuring several binaries
    /// notes each build before the rows measuring it ([`Self::note_build`]).
    build_identity: std::cell::RefCell<Option<BuildIdentity>>,
    /// Env var snapshot captured from the measurement request. Merged into
    /// every row's kv via `build_row` as `env.<NAME>` entries - lets
    /// env-gated code paths (A/B feature flags set from the shell) be
    /// distinguished across result rows.
    env_kv: Vec<crate::db::KvPair>,
    /// Unix epoch at which the current measurement loop began its first
    /// iteration; 0 until `MeasurementLoop` sets it.
    ///
    /// `prev.gap_seconds` has to be measured to the moment this run *started*,
    /// but it is written by `build_row` during `record` - which happens
    /// after every iteration has finished. Reading the clock there would fold
    /// this run's own duration into the gap, so a 10-minute cell launched
    /// immediately after its predecessor would report a ~10-minute gap and
    /// wrongly look cache-cold. A `Cell` because the `run_*` methods take
    /// `&self`.
    measure_start_epoch: std::cell::Cell<i64>,
}

/// The machine facts every results row carries.
struct HostFacts {
    hostname: String,
    kernel: Option<String>,
    governor: Option<String>,
    memory_available_mb: u64,
}

impl HostFacts {
    fn collect(hostname: &str) -> Self {
        Self {
            hostname: hostname.to_owned(),
            kernel: crate::host::kernel::collect().version,
            governor: crate::host::cpu::collect().governor,
            memory_available_mb: crate::host::memory::collect().available_mb,
        }
    }
}

/// How the measured binary was built, as a results row records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildIdentity {
    pub profile: BuildKind,
    /// The canonical feature spelling (sorted, duplicate-free), `None` for a
    /// default-features build.
    pub features: Option<String>,
    /// What else the build was made with (`crate::build_provenance`), stored
    /// as the row's `provenance.*` pairs.
    pub provenance: crate::build_provenance::BuildProvenance,
}

impl BuildIdentity {
    /// The identity of a cargo build brokkr ran: its provenance is read for
    /// the root that build ran in, which a separately built baseline crate
    /// does not share with the project.
    pub fn of(artifact: &crate::build::BuiltArtifact) -> Self {
        let mut features: Vec<&str> = artifact.features.iter().map(String::as_str).collect();
        features.sort_unstable();
        features.dedup();
        Self {
            profile: BuildKind::Cargo(artifact.profile.clone()),
            features: (!features.is_empty()).then(|| features.join(",")),
            provenance: crate::build_provenance::BuildProvenance::of(
                &artifact.build_root,
                &artifact.unit_profile,
            ),
        }
    }
}

impl BenchHarness {
    /// Create a new harness with a pre-acquired lock.
    ///
    /// Use this when the lock must be held before other work (e.g. cargo build)
    /// to prevent concurrent builds from contaminating timing. Git provenance
    /// comes from the build root (the code that was measured, a `--commit`
    /// worktree included); the results store lives under the project root.
    /// `included_config_files` are the composed config's `include` files,
    /// exempt from the dirty-tree refusal as `brokkr.toml` itself is.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_lock(
        lock: LockGuard,
        paths: &crate::config::ResolvedPaths,
        included_config_files: &[std::path::PathBuf],
        build_root: &crate::project::BuildRoot,
        project_root: &crate::project::ProjectRoot,
        project: crate::project::Project,
        force: bool,
        stop_marker: Option<String>,
    ) -> Result<Self, DevError> {
        std::fs::create_dir_all(&paths.scratch_dir)?;
        let host = HostFacts::collect(&paths.hostname);
        let git =
            crate::git::collect_with_config_paths(build_root.as_path(), included_config_files)?;
        let state = crate::clean::layout::ProjectState::new(project_root.as_path());
        std::fs::create_dir_all(state.root())?;
        let db = ResultsDb::open(&state.results_db())?;
        let storage_notes = format_storage_notes(&paths.drives);

        if !git.is_clean {
            if force {
                // Name the database. This governs results.db only - a gated
                // ratatoskr run still records into gate.db, and unqualified
                // the two lines read as a contradiction.
                console::warn("dirty tree - results will NOT be stored in results.db");
            } else {
                return Err(DevError::Preflight(vec![
                    "dirty tree - commit or stash changes before benchmarking".into(),
                    "run with --force to bench anyway (results will not be stored in results.db)"
                        .into(),
                ]));
            }
        }

        Ok(Self {
            lock,
            db,
            state,
            host,
            git,
            storage_notes,
            cargo_features: None,
            project,
            stop_marker,
            invocation: crate::db::RunInvocation {
                argv: Vec::new(),
                arm_key: String::new(),
            },
            measure_mode: crate::measure::RecordedMode::Bench,
            store_results: true,
            build_identity: std::cell::RefCell::new(None),
            env_kv: Vec::new(),
            measure_start_epoch: std::cell::Cell::new(0),
        })
    }

    /// Set the cargo features that were used to build the binary.
    ///
    /// When set, this value is used as the default `cargo_features` for all
    /// results recorded by this harness. Individual `BenchConfig.cargo_features`
    /// values override the harness default when set. The DB writer canonicalizes
    /// whichever label wins before persisting it.
    pub fn with_cargo_features(mut self, features: Option<String>) -> Self {
        self.cargo_features = features;
        self
    }

    /// Set the brokkr invocation for this harness. Every row takes its
    /// invocation from this session value.
    pub fn with_invocation(mut self, invocation: crate::db::RunInvocation) -> Self {
        self.invocation = invocation;
        self
    }

    /// Set the recorded measurement mode. Individual writers do not supply
    /// the measurement mode. `None` denotes run mode: report measurements
    /// without persisting result or sidecar records.
    pub fn with_measure_mode(mut self, mode: Option<crate::measure::RecordedMode>) -> Self {
        self.store_results = mode.is_some();
        if let Some(mode) = mode {
            self.measure_mode = mode;
        }
        self
    }

    /// Record what the session built. Rows built by this harness take their
    /// `cargo_profile` and `cargo_features` from it, unless a row names an
    /// external tool's identity itself ([`BenchConfig::external_build`]).
    pub fn with_build_identity(mut self, identity: BuildIdentity) -> Self {
        self.cargo_features.clone_from(&identity.features);
        self.build_identity = std::cell::RefCell::new(Some(identity));
        self
    }

    /// Record the build the rows that follow measure, for a suite that builds
    /// several binaries in one session (the project's own, a baseline crate).
    /// Rows recorded after it take its profile and provenance.
    pub fn note_build(&self, identity: BuildIdentity) {
        *self.build_identity.borrow_mut() = Some(identity);
    }

    /// Stop attributing a build: the rows that follow measure an executable
    /// brokkr did not build (an installed tool), so they record no provenance
    /// rather than the last build's.
    pub fn forget_build(&self) {
        *self.build_identity.borrow_mut() = None;
    }

    /// Attach captured env-var snapshot (see [`crate::config::captured_env_pairs`]).
    /// Merged into every row's kv via `build_row`.
    pub fn with_env_kv(mut self, env_kv: Vec<crate::db::KvPair>) -> Self {
        self.env_kv = env_kv;
        self
    }

    /// Warn when a stored mode's `--force` had no dirty tree to permit.
    /// `--force` never decides storage; a clean measured tree (a `--commit`
    /// worktree always is) stores regardless. Saying so keeps an A/B whose
    /// one arm is a clean worktree from reading as if both went unstored.
    pub(crate) fn note_unneeded_force(&self, force: bool) {
        if force && self.git.is_clean {
            console::warn("clean tree - --force has no effect, results will be stored in results.db");
        }
    }

    /// Expose the held lock so callers that spawn children outside of
    /// `run_external` (notably `BenchHarness::capture_hotpath`) can still report
    /// `child_pid` into the lockfile for `brokkr lock`.
    pub fn lock(&self) -> &LockGuard {
        &self.lock
    }

    /// Collect closure-provided timing using the shared measurement loop.
    pub fn run_internal<F>(&self, config: &BenchConfig, f: F) -> Result<BenchResult, DevError>
    where
        F: Fn(usize) -> Result<BenchResult, DevError>,
    {
        let mut measurement = self.measure(config, None, WallSource::Closure, false, |i, _| {
            f(i).map(Iteration::from_result)
        })?;
        self.record(config, &mut measurement, |_, _| Ok(()))
    }

    /// Collect captured profiling iterations, including their failure evidence.
    pub fn run_hotpath<F>(
        &self,
        config: &BenchConfig,
        program: &Path,
        f: F,
    ) -> Result<BenchResult, DevError>
    where
        F: Fn(usize, &mut crate::sidecar::SidecarFifo) -> Result<Iteration, DevError>,
    {
        let mut measurement = self.measure(
            config,
            Some(program),
            WallSource::External,
            true,
            |i, fifo| f(i, fifo.expect("profiled loop owns a FIFO")),
        )?;
        self.record(config, &mut measurement, |_, _| Ok(()))
    }

    /// External wall-clock over `invocation`. The row's `cli_args` is the
    /// invocation's own rendering, so the recorded command is the one that ran.
    pub(crate) fn run_external(
        &self,
        config: &BenchConfig,
        invocation: &Invocation<'_>,
        cwd: &Path,
    ) -> Result<BenchResult, DevError> {
        self.run_external_ok(config, invocation, cwd, &[])
    }

    pub(crate) fn run_external_ok(
        &self,
        config: &BenchConfig,
        invocation: &Invocation<'_>,
        cwd: &Path,
        ok_codes: &[i32],
    ) -> Result<BenchResult, DevError> {
        self.run_external_prepared(config, invocation, cwd, ok_codes, || Ok(()))
    }

    pub(crate) fn run_external_prepared<F>(
        &self,
        config: &BenchConfig,
        invocation: &Invocation<'_>,
        cwd: &Path,
        ok_codes: &[i32],
        prepare: F,
    ) -> Result<BenchResult, DevError>
    where
        F: Fn() -> Result<(), DevError>,
    {
        invocation.require_no_env("the external wall-clock runner")?;
        let config = config_for(config, invocation);
        let mut measurement = self.measure_external(
            &config,
            invocation.program(),
            &invocation.arg_refs(),
            cwd,
            ok_codes,
            WallSource::External,
            prepare,
        )?;
        self.record(&config, &mut measurement, |_, _| Ok(()))
    }

    /// Collect all closure durations and summarize the same ordered iterations.
    pub fn run_distribution<F>(&self, config: &BenchConfig, f: F) -> Result<BenchResult, DevError>
    where
        F: Fn(usize) -> Result<Duration, DevError>,
    {
        let mut measurement = self.measure(config, None, WallSource::Closure, false, |i, _| {
            let wall = f(i)?;
            Ok(Iteration::from_result(BenchResult {
                elapsed_ms: elapsed_to_ms(&wall),
                elapsed_us: Some(elapsed_to_us(&wall)),
                kv: Vec::new(),
                iterations: Vec::new(),
                distribution: None,
                hotpath: None,
            }))
        })?;
        self.record(config, &mut measurement, |result, iterations| {
            let mut samples: Vec<i64> = iterations
                .iter()
                .filter_map(|i| i.result.as_ref())
                .map(bench_ordering_key)
                .collect();
            samples.push(bench_ordering_key(result));
            result.distribution =
                summarize_distribution(&samples).map(|(distribution, _)| distribution);
            Ok(())
        })
    }

    pub(crate) fn run_external_with_kv(
        &self,
        config: &BenchConfig,
        invocation: &Invocation<'_>,
        cwd: &Path,
    ) -> Result<BenchResult, DevError> {
        invocation.require_no_env("the self-reported external runner")?;
        let config = config_for(config, invocation);
        let mut measurement = self.measure_external(
            &config,
            invocation.program(),
            &invocation.arg_refs(),
            cwd,
            &[],
            WallSource::SelfReported,
            || Ok(()),
        )?;
        self.record(&config, &mut measurement, |_, _| Ok(()))
    }

    /// Collect subprocess iterations without recording; postprocessing belongs in `record`.
    #[allow(clippy::too_many_arguments)]
    pub fn measure_external<F>(
        &self,
        config: &BenchConfig,
        program: &Path,
        args: &[&str],
        cwd: &Path,
        ok_codes: &[i32],
        source: WallSource,
        prepare: F,
    ) -> Result<Measurement, DevError>
    where
        F: Fn() -> Result<(), DevError>,
    {
        self.measure(config, Some(program), source, true, |i, fifo| {
            prepare()?;
            self.capture(
                program,
                args,
                cwd,
                &[],
                ok_codes,
                fifo.expect("external loop owns a FIFO"),
                i,
                None,
            )
        })
    }
}

/// `config` with `cli_args` taken from the invocation that is about to run,
/// so the row cannot record a different command from the one executed.
fn config_for(config: &BenchConfig, invocation: &Invocation<'_>) -> BenchConfig {
    BenchConfig {
        command: config.command.clone(),
        input_file: config.input_file.clone(),
        input_mb: config.input_mb,
        cargo_features: config.cargo_features.clone(),
        external_build: config.external_build.clone(),
        runs: config.runs,
        cli_args: Some(invocation.cli_args()),
        metadata: config.metadata.clone(),
    }
}

mod argv;
pub(crate) use argv::{Executable, ExternalTool, Invocation, ResolvedExecutable};
pub use argv::format_cli_args;
pub(crate) mod capture;
pub(crate) mod execution;
pub(crate) mod kv;
mod record;
pub(crate) mod wall;

#[cfg(test)]
mod tests;
