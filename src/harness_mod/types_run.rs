use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::build::CargoProfile;
use crate::config::DriveConfig;
use crate::db::{
    self, Distribution, HotpathData, KvClass, KvPair, KvValue, ResultsDb, RunRow, Wall,
};
use crate::error::DevError;
use crate::git::GitInfo;
use crate::lockfile::LockGuard;
use crate::output::{child, console};

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
    pub external_build: Option<CargoProfile>,
    pub runs: usize,
    /// Literal subprocess invocation (pbfhogg/elivagar/...). Populated by
    /// dispatch from the argv it hands to the tool binary.
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
    /// `cargo_profile` and canonical `cargo_features` come from it.
    build_identity: Option<BuildIdentity>,
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
    pub profile: CargoProfile,
    /// The canonical feature spelling (sorted, duplicate-free), `None` for a
    /// default-features build.
    pub features: Option<String>,
}

impl BuildIdentity {
    /// The identity of a cargo build brokkr ran.
    pub fn of(artifact: &crate::build::BuiltArtifact) -> Self {
        let mut features: Vec<&str> = artifact.features.iter().map(String::as_str).collect();
        features.sort_unstable();
        features.dedup();
        Self {
            profile: artifact.profile,
            features: (!features.is_empty()).then(|| features.join(",")),
        }
    }
}

impl BenchHarness {
    /// Create a new harness with a pre-acquired lock.
    ///
    /// Use this when the lock must be held before other work (e.g. cargo build)
    /// to prevent concurrent builds from contaminating timing.
    pub fn new_with_lock(
        lock: LockGuard,
        paths: &crate::config::ResolvedPaths,
        project_root: &Path,
        db_root: Option<&Path>,
        project: crate::project::Project,
        force: bool,
        stop_marker: Option<String>,
    ) -> Result<Self, DevError> {
        std::fs::create_dir_all(&paths.scratch_dir)?;
        let host = HostFacts::collect(&paths.hostname);
        let git = crate::git::collect(project_root)?;
        let state = crate::clean::layout::ProjectState::new(db_root.unwrap_or(project_root));
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
            invocation: crate::db::RunInvocation { argv: Vec::new(), arm_key: String::new() },
            measure_mode: crate::measure::RecordedMode::Bench,
            store_results: true,
            build_identity: None,
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
        self.build_identity = Some(identity);
        self
    }

    /// Attach captured env-var snapshot (see [`crate::config::captured_env_pairs`]).
    /// Merged into every row's kv via `build_row`.
    pub fn with_env_kv(mut self, env_kv: Vec<crate::db::KvPair>) -> Self {
        self.env_kv = env_kv;
        self
    }

    /// Expose the held lock so callers that spawn children outside of
    /// `run_external` (notably `BenchHarness::capture_hotpath`) can still report
    /// `child_pid` into the lockfile for `brokkr lock`.
    pub fn lock(&self) -> &LockGuard {
        &self.lock
    }

    /// Collect closure-provided timing using the shared measurement loop.
    pub fn run_internal<F>(&self, config: &BenchConfig, f: F) -> Result<BenchResult, DevError>
    where F: Fn(usize) -> Result<BenchResult, DevError>,
    {
        let mut measurement = self.measure(config, None, WallSource::Closure, false,
            |i, _| f(i).map(Iteration::from_result))?;
        self.record(config, &mut measurement, |_, _| Ok(()))
    }

    /// Collect captured profiling iterations, including their failure evidence.
    pub fn run_hotpath<F>(&self, config: &BenchConfig, program: &Path, f: F)
        -> Result<BenchResult, DevError>
    where F: Fn(usize, &mut crate::sidecar::SidecarFifo) -> Result<Iteration, DevError>,
    {
        let mut measurement = self.measure(config, Some(program), WallSource::External, true,
            |i, fifo| f(i, fifo.expect("profiled loop owns a FIFO")))?;
        self.record(config, &mut measurement, |_, _| Ok(()))
    }

    pub fn run_external(&self, config: &BenchConfig, program: &Path, args: &[&str], cwd: &Path)
        -> Result<BenchResult, DevError>
    {
        self.run_external_ok(config, program, args, cwd, &[])
    }

    pub fn run_external_ok(&self, config: &BenchConfig, program: &Path, args: &[&str], cwd: &Path,
        ok_codes: &[i32]) -> Result<BenchResult, DevError>
    {
        self.run_external_prepared(config, program, args, cwd, ok_codes, || Ok(()))
    }

    pub(crate) fn run_external_prepared<F>(&self, config: &BenchConfig, program: &Path,
        args: &[&str], cwd: &Path, ok_codes: &[i32], prepare: F) -> Result<BenchResult, DevError>
    where F: Fn() -> Result<(), DevError>,
    {
        let mut measurement = self.measure_external(config, program, args, cwd, ok_codes,
            WallSource::External, prepare)?;
        self.record(config, &mut measurement, |_, _| Ok(()))
    }

    /// Collect all closure durations and summarize the same ordered iterations.
    pub fn run_distribution<F>(&self, config: &BenchConfig, f: F) -> Result<BenchResult, DevError>
    where F: Fn(usize) -> Result<Duration, DevError>,
    {
        let mut measurement = self.measure(config, None, WallSource::Closure, false, |i, _| {
            let wall = f(i)?;
            Ok(Iteration::from_result(BenchResult {
                elapsed_ms: elapsed_to_ms(&wall), elapsed_us: Some(elapsed_to_us(&wall)),
                kv: Vec::new(), iterations: Vec::new(), distribution: None, hotpath: None,
            }))
        })?;
        self.record(config, &mut measurement, |result, iterations| {
            let mut samples: Vec<i64> = iterations.iter().filter_map(|i| i.result.as_ref())
                .map(bench_ordering_key).collect();
            samples.push(bench_ordering_key(result));
            result.distribution = summarize_distribution(&samples).map(|(distribution, _)| distribution);
            Ok(())
        })
    }

    pub fn run_external_with_kv(&self, config: &BenchConfig, program: &Path, args: &[&str], cwd: &Path)
        -> Result<BenchResult, DevError>
    {
        let mut measurement = self.measure_external(config, program, args, cwd, &[],
            WallSource::SelfReported, || Ok(()))?;
        self.record(config, &mut measurement, |_, _| Ok(()))
    }

    /// Collect subprocess iterations without recording; postprocessing belongs in `record`.
    #[allow(clippy::too_many_arguments)]
    pub fn measure_external<F>(&self, config: &BenchConfig, program: &Path, args: &[&str],
        cwd: &Path, ok_codes: &[i32], source: WallSource, prepare: F)
        -> Result<Measurement, DevError>
    where F: Fn() -> Result<(), DevError>,
    {
        self.measure(config, Some(program), source, true, |i, fifo| {
            prepare()?;
            self.capture(program, args, cwd, &[], ok_codes,
                fifo.expect("external loop owns a FIFO"), i, None)
        })
    }

    // -----------------------------------------------------------------------
    // Private methods
    // -----------------------------------------------------------------------

    /// Write and report the winning row; `record` owns sidecar persistence.
    fn record_measured(
        &self,
        config: &BenchConfig,
        result: &BenchResult,
        iterations_us: &[i64],
        fired_stop_marker: Option<&str>,
    ) -> Result<Option<String>, DevError> {
        let mode = self.effective_mode();
        if !self.store_results {
            force_emit_result_lines(config, mode.as_deref(), result, &self.git);
            return Ok(None);
        }
        if self.git.is_clean {
            let row = self.build_row(config, result, iterations_us, fired_stop_marker)?;
            let (uuid, short) = match self.db.insert(&row) {
                Ok(pair) => pair,
                Err(e) => {
                    force_emit_result_lines(config, mode.as_deref(), result, &self.git);
                    console::error(&format!("results.db insert failed - result not stored: {e}"));
                    return Err(e);
                }
            };
            emit_result_lines(config, mode.as_deref(), result, &self.git);
            // The one line naming the stored row's id (also the sidecar
            // profile's). Not quiet-gated: it is how the run is found again.
            console::run_msg(&format!("results.db {short}"));
            Ok(Some(uuid))
        } else {
            // Dirty tree: no DB insert, no UUID. Always print result line
            // since the data can't be looked up later.
            force_emit_result_lines(config, mode.as_deref(), result, &self.git);
            Ok(None)
        }
    }

    /// The measurement mode a row, `[result]` line and sidecar record
    /// carry: the session's `measure_mode`, set from the request.
    pub fn effective_mode(&self) -> Option<String> {
        self.store_results.then(|| self.measure_mode.label().to_owned())
    }

    /// Build a `RunInfo` from harness state for sidecar provenance.
    fn build_run_info(
        &self,
        config: &BenchConfig,
        program: &Path,
        start_epoch: i64,
        binary_xxh128: Option<String>,
        pid: Option<u32>,
        ending: Option<child::Ending>,
    ) -> crate::db::sidecar::RunInfo {
        crate::db::sidecar::RunInfo {
            run_start_epoch: Some(start_epoch),
            pid: pid.map(i64::from),
            command: Some(config.command.clone()),
            binary_path: Some(program.display().to_string()),
            binary_xxh128,
            git_commit: Some(self.git.commit.clone()),
            mode: self.effective_mode(),
            dataset: config.input_file.clone(),
            ending,
        }
    }

    /// Store sidecar data in the separate sidecar.db.
    ///
    /// `best_run_idx` records which of the N runs produced the reported
    /// elapsed_ms, so `brokkr sidecar <uuid>` defaults to the right run.
    ///
    /// `run_info` carries provenance metadata (timestamp, binary hash, git
    /// commit, mode, dataset, ending) so that `brokkr sidecar dirty`
    /// can identify exactly which run produced the sidecar data.
    ///
    /// When `uuid` is `None` (dirty tree or failed run), generates a fresh
    /// UUID and updates the `dirty` latest pointer so `brokkr results dirty`
    /// always finds the most recent unstored run.
    fn store_sidecar(
        &self,
        uuid: Option<&str>,
        sidecar_runs: &[&crate::sidecar::SidecarData],
        best_run_idx: usize,
        run_info: Option<&crate::db::sidecar::RunInfo>,
    ) -> Result<(), DevError> {
        if !self.store_results {
            return Ok(());
        }

        let sidecar_db_path = self.state.sidecar_db();
        let sidecar_db = crate::db::sidecar::SidecarDb::open(&sidecar_db_path)?;

        let (store_uuid, is_dirty) = match uuid {
            Some(u) => (u.to_owned(), false),
            None => {
                // Generate a unique UUID for this dirty/failed run.
                (crate::db::generate_uuid()?, true)
            }
        };

        for (i, data) in sidecar_runs.iter().enumerate() {
            sidecar_db.store_run(&store_uuid, i, data)?;
        }

        sidecar_db.store_meta(&store_uuid, best_run_idx, sidecar_runs.len(), run_info)?;

        if is_dirty {
            sidecar_db.set_latest("dirty", &store_uuid)?;
        }

        // The one sidecar summary line: what was stored (and the id, only for
        // a `dirty`-filed profile - otherwise the results.db line named it).
        // Per-run attach and sample counts are run-log detail (the sidecar run);
        // the wall time is on the `[result]` line.
        let short = &store_uuid[..8.min(store_uuid.len())];
        console::sidecar_msg(&sidecar_summary_line(
            short,
            is_dirty,
            sidecar_runs,
            best_run_idx,
        ));

        // Close the writer before backup. The backup API reads the logical
        // DB state regardless, but closing avoids holding two connections.
        drop(sidecar_db);

        // Back up sidecar DB while the lock is still held.
        if let Err(e) = backup_sidecar(&sidecar_db_path, self.project) {
            // A warning, not narration: the quiet-gated sidecar line hid a
            // real loss of the backup.
            console::warn(&format!("sidecar backup failed (non-fatal): {e}"));
        }

        Ok(())
    }

    /// Build a `RunRow` from harness state, config, and result.
    fn build_row(
        &self,
        config: &BenchConfig,
        result: &BenchResult,
        iterations_us: &[i64],
        fired_stop_marker: Option<&str>,
    ) -> Result<RunRow, DevError> {
        let mut kv = config.metadata.clone();
        // Sources are appended in increasing precedence - metadata, env
        // capture, prev.*, then the run's own counters - and the insert
        // keeps the LAST pair per key (`db::write::insert_inner`), so
        // runtime counters win the unlikely collision (env.foo vs a
        // runtime env.foo).
        kv.extend(self.env_kv.iter().cloned());
        kv.extend(previous_run_kv(&self.db, self.measure_start_epoch.get())?);
        let mut peak_rss_mb: Option<f64> = None;
        for pair in &result.kv {
            if pair.key == "peak_rss_kb" {
                if let KvValue::Int(kb) = &pair.value {
                    #[allow(clippy::cast_precision_loss)]
                    {
                        peak_rss_mb = Some(*kb as f64 / 1024.0);
                    }
                }
                continue; // promoted to column, don't duplicate in run_kv
            }
            kv.push(pair.clone());
        }

        // Harness-level values take precedence. The individual writers
        // don't have to know or care about the measurement mode or the
        // brokkr invocation string - the harness attaches them.
        let mode = self.effective_mode();

        Ok(RunRow {
            hostname: self.host.hostname.clone(),
            commit: self.git.commit.clone(),
            subject: self.git.subject.clone(),
            command: config.command.clone(),
            mode,
            input_file: config.input_file.clone(),
            input_mb: config.input_mb,
            peak_rss_mb,
            cargo_features: config
                .cargo_features
                .clone()
                .or_else(|| self.cargo_features.clone()),
            // An external tool's own identity, else what the session built.
            // A session that measures a binary it did not build (a server the
            // command started, a suite's own release builds) records release,
            // the profile every such build path uses.
            cargo_profile: config
                .external_build
                .or_else(|| self.build_identity.as_ref().map(|b| b.profile))
                .unwrap_or(CargoProfile::Release),
            // Carried through, not recomputed: only the harness path that
            // produced the result knows whether sub-millisecond timing was
            // ever measured. No microseconds means "never measured", which
            // is not the same as `elapsed_ms * 1000`.
            elapsed: result_wall(result.elapsed_ms, result.elapsed_us),
            kernel: self.host.kernel.clone(),
            cpu_governor: self.host.governor.clone(),
            avail_memory_mb: i64::try_from(self.host.memory_available_mb).ok(),
            storage_notes: self.storage_notes.clone(),
            cli_args: config.cli_args.clone(),
            invocation: self.invocation.clone(),
            project: self.project.name().to_owned(),
            stop_marker: if self.stop_marker.is_some() || fired_stop_marker.is_some() {
                Some((
                    self.stop_marker.clone(),
                    fired_stop_marker.map(str::to_owned),
                ))
            } else {
                None
            },
            kv,
            // Microsecond walls are all-or-none (`all_or_none`), so either
            // every iteration is exact or every one is millisecond-only.
            iterations: if iterations_us.len() == result.iterations.len() {
                iterations_us.iter().copied().map(Wall::from_micros).collect()
            } else {
                result.iterations.iter().copied().map(Wall::from_legacy_millis).collect()
            },
            distribution: result.distribution.clone(),
            hotpath: result.hotpath.clone(),
        })
    }
}

/// Describe the run recorded just before this one as `prev.*` kv pairs.
///
/// Records what the machine was doing immediately beforehand, which is the
/// cheapest available proxy for page-cache state: a cell that reads the same
/// file its predecessor just read starts warm, and reports a wall the code
/// didn't earn. `prev.gap_s` and `prev.input_file` together are usually enough
/// to tell "this baseline was warm" from "this baseline was fast".
///
/// Must be called before the current row is inserted, or it describes the
/// current run. Lookup errors are returned so the row cannot silently lose
/// predecessor provenance.
///
/// `measure_start_epoch` is when this run began; the gap is measured to that,
/// not to now, because "now" is after every iteration has run and would count
/// this run's own duration as idle time. Falls back to the current clock when
/// it is 0 (a caller that recorded a result without going through a `run_*`
/// loop), which is the same reading as before the loop for an instant record.
fn previous_run_kv(
    db: &ResultsDb,
    measure_start_epoch: i64,
) -> Result<Vec<KvPair>, DevError> {
    let Some(prev) = db.previous_run()? else {
        return Ok(Vec::new());
    };
    let start = if measure_start_epoch > 0 {
        measure_start_epoch
    } else {
        wall_clock_epoch()
    };
    // `prev.gap_seconds`, not `prev.gap_s`: the single-result view renders kv
    // keys with underscores as spaces, and "prev.gap s" reads as a typo.
    // Clamped at 0 - a backwards system clock shouldn't record a negative gap.
    let gap_seconds = (start - prev.timestamp_epoch).max(0);
    let mut kv = vec![
        KvPair::text(KvClass::Predecessor.qualify("command"), prev.command),
        KvPair::int(
            KvClass::Predecessor.qualify("gap_seconds"),
            gap_seconds,
        ),
    ];
    if !prev.uuid.is_empty() {
        // Short form, matching what `brokkr results` prints and accepts as a
        // prefix - so the recorded value is directly pasteable.
        kv.push(KvPair::text(
            KvClass::Predecessor.qualify("uuid"),
            &prev.uuid[..8.min(prev.uuid.len())],
        ));
    }
    if let Some(input) = prev.input_file {
        kv.push(KvPair::text(
            KvClass::Predecessor.qualify("input_file"),
            input,
        ));
    }
    Ok(kv)
}
