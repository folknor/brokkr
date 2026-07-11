use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::build::CargoProfile;
use crate::config::DriveConfig;
use crate::db::{self, Distribution, HotpathData, KvPair, KvValue, ResultsDb, RunRow};
use crate::env::EnvInfo;
use crate::error::DevError;
use crate::git::GitInfo;
use crate::lockfile::LockGuard;
use crate::output;

// ---------------------------------------------------------------------------
// Configuration and result types
// ---------------------------------------------------------------------------

/// Configuration for a benchmark run.
pub struct BenchConfig {
    pub command: String,
    /// Measurement mode override (`"bench"`/`"hotpath"`/`"alloc"`).
    /// Usually left `None`; the harness fills it from its
    /// `measure_mode` field, set via `with_measure_mode` at
    /// construction. Individual writers only set this when they need
    /// to override the harness default.
    pub mode: Option<String>,
    pub input_file: Option<String>,
    pub input_mb: Option<f64>,
    pub cargo_features: Option<String>,
    pub cargo_profile: CargoProfile,
    pub runs: usize,
    /// Literal subprocess invocation (pbfhogg/elivagar/...). Populated by
    /// dispatch from the argv it hands to the tool binary.
    pub cli_args: Option<String>,
    /// Literal `brokkr <...>` invocation (std::env::args joined). Populated
    /// by main.rs and threaded through `MeasureRequest`. Stored parallel to
    /// `cli_args` so queries can grep either what the user asked brokkr to
    /// do or what brokkr asked the tool to do.
    pub brokkr_args: Option<String>,
    pub metadata: Vec<KvPair>,
}

/// Result of a single benchmark measurement.
pub struct BenchResult {
    pub elapsed_ms: i64,
    pub kv: Vec<KvPair>,
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
    db_dir: std::path::PathBuf,
    env: EnvInfo,
    git: GitInfo,
    storage_notes: Option<String>,
    cargo_features: Option<String>,
    project: crate::project::Project,
    stop_marker: Option<String>,
    /// Literal `brokkr <...>` invocation (std::env::args joined). Set once
    /// per harness via `with_brokkr_args`; every row built by this harness
    /// inherits it via `build_row`. Lets individual bench writers stay
    /// agnostic of the brokkr-level invocation.
    brokkr_args: Option<String>,
    /// Measurement mode string (`"bench"`, `"hotpath"`, `"alloc"`). Set
    /// once per harness via `with_measure_mode`. Overrides
    /// `BenchConfig.variant` when set - individual bench writers only
    /// need to set `variant` when they mean to override (they almost
    /// never do post-v13).
    measure_mode: Option<String>,
    /// Env var snapshot captured at `with_request` time. Merged into
    /// every row's kv via `build_row` as `env.<NAME>` entries - lets
    /// env-gated code paths (A/B feature flags set from the shell) be
    /// distinguished across result rows.
    env_kv: Vec<crate::db::KvPair>,
}

impl BenchHarness {
    /// Create a new harness, acquiring the lockfile and collecting environment.
    ///
    /// When `db_root` is `Some`, the results DB is opened from that directory
    /// instead of `project_root`. This is used for worktree-based benchmarking
    /// where git info comes from the worktree but results are stored in the
    /// main tree's database.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        paths: &crate::config::ResolvedPaths,
        project_root: &Path,
        db_root: Option<&Path>,
        project: crate::project::Project,
        lock_command: &str,
        force: bool,
        stop_marker: Option<String>,
    ) -> Result<Self, DevError> {
        let lock = crate::lockfile::acquire(&crate::lockfile::LockContext {
            project: project.name(),
            command: lock_command,
            project_root: &project_root.display().to_string(),
        })?;
        Self::new_with_lock(lock, paths, project_root, db_root, project, force, stop_marker)
    }

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
        let env = crate::env::collect(paths, project, project_root);
        let git = crate::git::collect(project_root)?;
        let db_base = db_root.unwrap_or(project_root);
        let db_dir = db_base.join(".brokkr");
        std::fs::create_dir_all(&db_dir)?;
        let db = ResultsDb::open(&db_dir.join("results.db"))?;
        let storage_notes = format_storage_notes(&paths.drives);

        if !git.is_clean {
            if force {
                output::warn("dirty tree - results will NOT be stored in database");
            } else {
                return Err(DevError::Preflight(vec![
                    "dirty tree - commit or stash changes before benchmarking".into(),
                    "run with --force to bench anyway (results will not be stored)".into(),
                ]));
            }
        }

        Ok(Self {
            lock,
            db,
            db_dir,
            env,
            git,
            storage_notes,
            cargo_features: None,
            project,
            stop_marker,
            brokkr_args: None,
            measure_mode: None,
            env_kv: Vec::new(),
        })
    }

    /// Set the cargo features that were used to build the binary.
    ///
    /// When set, this value is used as the default `cargo_features` for all
    /// results recorded by this harness. Individual `BenchConfig.cargo_features`
    /// values override the harness default when set.
    pub fn with_cargo_features(mut self, features: Option<String>) -> Self {
        self.cargo_features = features;
        self
    }

    /// Set the literal `brokkr <...>` invocation string for this harness.
    /// All rows recorded by this harness will carry it in the `brokkr_args`
    /// column unless the individual `BenchConfig` overrides it explicitly.
    pub fn with_brokkr_args(mut self, args: String) -> Self {
        self.brokkr_args = Some(args);
        self
    }

    /// Set the measurement mode string (`"bench"`, `"hotpath"`, `"alloc"`).
    /// This overrides whatever `BenchConfig.variant` is set to, so
    /// individual writers don't have to supply it.
    pub fn with_measure_mode(mut self, mode: Option<&str>) -> Self {
        self.measure_mode = mode.map(str::to_owned);
        self
    }

    /// Attach captured env-var snapshot (see [`crate::config::captured_env_pairs`]).
    /// Merged into every row's kv via `build_row`.
    pub fn with_env_kv(mut self, env_kv: Vec<crate::db::KvPair>) -> Self {
        self.env_kv = env_kv;
        self
    }

    /// Expose the held lock so callers that spawn children outside of
    /// `run_external` (notably `run_hotpath_capture`) can still report
    /// `child_pid` into the lockfile for `brokkr lock`.
    pub fn lock(&self) -> &LockGuard {
        &self.lock
    }

    /// Internal timing: closure called N times, returns `BenchResult`.
    /// Best-of-N (minimum `elapsed_ms`).
    pub fn run_internal<F>(&self, config: &BenchConfig, f: F) -> Result<BenchResult, DevError>
    where
        F: Fn(usize) -> Result<BenchResult, DevError>,
    {
        let mut best: Option<BenchResult> = None;
        let total = clamp_u32(config.runs);

        for i in 0..config.runs {
            output::bench_msg(&format!("run {}/{}", i + 1, config.runs));
            self.lock.set_progress(clamp_u32(i + 1), total);
            let result = f(i)?;
            best = Some(pick_best(best, result));
        }

        let best =
            best.ok_or_else(|| DevError::Config("benchmark requires at least 1 run".into()))?;

        self.record_result(config, &best)?;
        Ok(best)
    }

    /// External timing: run subprocess N times with sidecar monitoring.
    ///
    /// The sidecar samples `/proc` metrics at 100ms intervals and reads phase
    /// markers from a FIFO. Sidecar data is stored in `.brokkr/sidecar.db`.
    ///
    /// The sidecar takes ownership of each `Child` process, drains
    /// stdout/stderr in background threads (preventing pipe-buffer deadlock),
    /// and records the child's exact exit time for wall-clock measurement.
    pub fn run_external(
        &self,
        config: &BenchConfig,
        program: &Path,
        args: &[&str],
        cwd: &Path,
    ) -> Result<BenchResult, DevError> {
        self.run_external_ok(config, program, args, cwd, &[])
    }

    /// Like `run_external`, but treats the given exit codes as success.
    /// For example, `diff` exits 1 when differences are found (not an error).
    pub fn run_external_ok(
        &self,
        config: &BenchConfig,
        program: &Path,
        args: &[&str],
        cwd: &Path,
        ok_codes: &[i32],
    ) -> Result<BenchResult, DevError> {
        let scratch_dir = &self.db_dir;
        use crate::sidecar;

        let start_epoch = wall_clock_epoch();
        let mut fifo = sidecar::SidecarFifo::create(scratch_dir)?;
        let fifo_path_str = fifo.path_str()?.to_owned();

        let mut best_ms: Option<i64> = None;
        let mut best_run_idx: usize = 0;
        let mut last_pid: u32 = 0;
        let mut sidecar_runs: Vec<sidecar::SidecarData> = Vec::with_capacity(config.runs);
        let prog_str = program.display().to_string();
        let total = clamp_u32(config.runs);

        for i in 0..config.runs {
            output::bench_msg(&format!("run {}/{}", i + 1, config.runs));
            self.lock.set_progress(clamp_u32(i + 1), total);

            // Reopen FIFO read end between runs so the next child's write
            // end connects to a fresh reader (not one stuck at EOF).
            if i > 0 {
                fifo.reopen()?;
            }

            let env = [("BROKKR_MARKER_FIFO", fifo_path_str.as_str())];
            let start = Instant::now();
            let child = output::spawn_captured(&prog_str, args, cwd, &env, true)?;
            last_pid = child.id();
            self.lock.set_child_pid(last_pid);

            // run_sidecar takes ownership of the child, drains stdout/stderr
            // in background threads, and returns everything when the child exits.
            let result = sidecar::run_sidecar(child, &mut fifo, i, start, self.stop_marker.as_deref());
            // Iteration's child has reaped; clear so a stale PID can't
            // be SIGKILLed by `--hard` once the kernel recycles it.
            self.lock.clear_child_pid();
            let stopped = result.stopped_by_marker;
            let interrupted = result.stopped_by_signal;

            let captured = output::CapturedOutput {
                status: result.exit_status,
                stdout: result.stdout,
                stderr: result.stderr,
                elapsed: result.elapsed,
            };
            let ms = elapsed_to_ms(&captured.elapsed);

            // Always collect sidecar data - especially valuable when the
            // child is OOM-killed, since the /proc trajectory shows what
            // happened before the kill.
            sidecar_runs.push(result.data);

            // `brokkr kill` caught SIGTERM and we killed the child in the
            // sidecar loop. Save the partial data under `dirty` and bail.
            if interrupted {
                drop(fifo);
                let exit_code = exit_code_from_status(&captured.status);
                let info = self.build_run_info(config, program, start_epoch, last_pid, Some(exit_code));
                self.store_sidecar(None, &sidecar_runs, i, Some(&info)).ok();
                return Err(DevError::Interrupted);
            }

            // When the child was killed by a --stop marker, the exit status
            // is SIGKILL - not a real failure.
            if !stopped {
                let exit_err = captured.check_success_or(&prog_str, ok_codes).err();
                if let Some(e) = exit_err {
                    drop(fifo);
                    let exit_code = exit_code_from_status(&captured.status);
                    let info = self.build_run_info(config, program, start_epoch, last_pid, Some(exit_code));
                    self.store_sidecar(None, &sidecar_runs, i, Some(&info)).ok();
                    return Err(e);
                }
            }

            if best_ms.is_none_or(|best| ms < best) {
                best_ms = Some(ms);
                best_run_idx = i;
            }
        }

        drop(fifo);

        let elapsed_ms =
            best_ms.ok_or_else(|| DevError::Config("benchmark requires at least 1 run".into()))?;

        let bench_result = BenchResult {
            elapsed_ms,
            kv: Vec::new(),
            distribution: None,
            hotpath: None,
        };

        let uuid = self.record_result(config, &bench_result)?;
        let info = self.build_run_info(config, program, start_epoch, last_pid, Some(0));
        self.store_sidecar(uuid.as_deref(), &sidecar_runs, best_run_idx, Some(&info))?;

        Ok(bench_result)
    }

    /// Distribution timing: collect all N samples, compute min/p50/p95/max.
    pub fn run_distribution<F>(&self, config: &BenchConfig, f: F) -> Result<BenchResult, DevError>
    where
        F: Fn(usize) -> Result<i64, DevError>,
    {
        let mut samples = Vec::with_capacity(config.runs);
        let total = clamp_u32(config.runs);

        for i in 0..config.runs {
            output::bench_msg(&format!("run {}/{}", i + 1, config.runs));
            self.lock.set_progress(clamp_u32(i + 1), total);
            let ms = f(i)?;
            samples.push(ms);
        }

        samples.sort_unstable();

        let min = percentile(&samples, 0);
        let p50 = percentile(&samples, 50);
        let p95 = percentile(&samples, 95);
        let max = percentile(&samples, 100);

        #[allow(clippy::cast_possible_wrap)]
        let dist = Distribution {
            samples: samples.len() as i64,
            min_ms: min,
            p50_ms: p50,
            p95_ms: p95,
            max_ms: max,
        };

        let result = BenchResult {
            elapsed_ms: min,
            kv: Vec::new(),
            distribution: Some(dist),
            hotpath: None,
        };

        self.record_result(config, &result)?;
        Ok(result)
    }

    /// External timing with kv parsing and sidecar: run subprocess N times,
    /// parse stderr for key=value lines. Uses the subprocess's self-reported
    /// `elapsed_ms` from stderr (not external wall-clock). Best-of-N.
    pub fn run_external_with_kv(
        &self,
        config: &BenchConfig,
        program: &Path,
        args: &[&str],
        cwd: &Path,
    ) -> Result<BenchResult, DevError> {
        let (best, _stderr) = self.run_external_with_kv_raw(config, program, args, cwd)?;
        self.record_result(config, &best)?;
        Ok(best)
    }

    /// Like `run_external_with_kv` but does NOT record - returns the best
    /// result and the raw stderr from the best run. Caller is responsible for
    /// calling `record_result` after any post-processing.
    pub fn run_external_with_kv_raw(
        &self,
        config: &BenchConfig,
        program: &Path,
        args: &[&str],
        cwd: &Path,
    ) -> Result<(BenchResult, Vec<u8>), DevError> {
        let scratch_dir = &self.db_dir;
        use crate::sidecar;

        let start_epoch = wall_clock_epoch();
        let mut fifo = sidecar::SidecarFifo::create(scratch_dir)?;
        let fifo_path_str = fifo.path_str()?.to_owned();

        let mut best: Option<BenchResult> = None;
        let mut best_stderr: Vec<u8> = Vec::new();
        let mut best_run_idx: usize = 0;
        let mut last_pid: u32 = 0;
        let mut sidecar_runs: Vec<sidecar::SidecarData> = Vec::with_capacity(config.runs);
        let prog_str = program.display().to_string();
        let total = clamp_u32(config.runs);

        for i in 0..config.runs {
            output::bench_msg(&format!("run {}/{}", i + 1, config.runs));
            self.lock.set_progress(clamp_u32(i + 1), total);

            if i > 0 {
                fifo.reopen()?;
            }

            let env = [("BROKKR_MARKER_FIFO", fifo_path_str.as_str())];
            let start = Instant::now();
            let child = output::spawn_captured(&prog_str, args, cwd, &env, true)?;
            last_pid = child.id();
            self.lock.set_child_pid(last_pid);
            let sidecar_result = sidecar::run_sidecar(child, &mut fifo, i, start, self.stop_marker.as_deref());
            self.lock.clear_child_pid();
            let stopped = sidecar_result.stopped_by_marker;
            let interrupted = sidecar_result.stopped_by_signal;

            let captured = output::CapturedOutput {
                status: sidecar_result.exit_status,
                stdout: sidecar_result.stdout,
                stderr: sidecar_result.stderr,
                elapsed: sidecar_result.elapsed,
            };

            sidecar_runs.push(sidecar_result.data);

            if interrupted {
                drop(fifo);
                let exit_code = exit_code_from_status(&captured.status);
                let info = self.build_run_info(config, program, start_epoch, last_pid, Some(exit_code));
                self.store_sidecar(None, &sidecar_runs, i, Some(&info)).ok();
                return Err(DevError::Interrupted);
            }

            if !stopped {
                let exit_err = captured.check_success(&prog_str).err();
                if let Some(err) = exit_err {
                    drop(fifo);
                    let exit_code = exit_code_from_status(&captured.status);
                    let info = self.build_run_info(config, program, start_epoch, last_pid, Some(exit_code));
                    self.store_sidecar(None, &sidecar_runs, i, Some(&info)).ok();
                    return Err(err);
                }
            }

            let result = parse_kv_stderr(&captured.stderr)?;
            let is_new_best = best
                .as_ref()
                .is_none_or(|b| result.elapsed_ms < b.elapsed_ms);
            if is_new_best {
                best_stderr = captured.stderr;
                best_run_idx = i;
            }
            best = Some(pick_best(best, result));
        }

        drop(fifo);

        let best =
            best.ok_or_else(|| DevError::Config("benchmark requires at least 1 run".into()))?;

        let info = self.build_run_info(config, program, start_epoch, last_pid, Some(0));
        self.store_sidecar(None, &sidecar_runs, best_run_idx, Some(&info))?;

        Ok((best, best_stderr))
    }

    // -----------------------------------------------------------------------
    // Private methods
    // -----------------------------------------------------------------------

    /// Record a result: always emit to stdout, store in DB if tree is clean.
    /// Prints the short UUID to stdout (always, regardless of quiet mode).
    /// Returns the full UUID if stored, `None` if the tree was dirty.
    pub fn record_result(
        &self,
        config: &BenchConfig,
        result: &BenchResult,
    ) -> Result<Option<String>, DevError> {
        if self.git.is_clean {
            let row = self.build_row(config, result);
            let (uuid, short) = self.db.insert(&row)?;
            emit_result_lines(config, result, &self.git);
            output::bench_msg(&format!("stored in results.db ({short})"));
            println!("{short}");
            Ok(Some(uuid))
        } else {
            // Dirty tree: no DB insert, no UUID. Always print result line
            // since the data can't be looked up later.
            force_emit_result_lines(config, result, &self.git);
            Ok(None)
        }
    }

    /// Build a `RunInfo` from harness state for sidecar provenance.
    fn build_run_info(
        &self,
        config: &BenchConfig,
        program: &Path,
        start_epoch: i64,
        pid: u32,
        exit_code: Option<i32>,
    ) -> crate::db::sidecar::RunInfo {
        let binary_xxh128 = crate::preflight::compute_xxh128(program).ok();
        crate::db::sidecar::RunInfo {
            run_start_epoch: Some(start_epoch),
            pid: Some(i64::from(pid)),
            command: Some(config.command.clone()),
            binary_path: Some(program.display().to_string()),
            binary_xxh128,
            git_commit: Some(self.git.commit.clone()),
            mode: config.mode.clone(),
            dataset: config.input_file.clone(),
            exit_code,
        }
    }

    /// Store sidecar data in the separate sidecar.db.
    ///
    /// `best_run_idx` records which of the N runs produced the reported
    /// elapsed_ms, so `brokkr sidecar <uuid>` defaults to the right run.
    ///
    /// `run_info` carries provenance metadata (timestamp, binary hash, git
    /// commit, variant, dataset, exit code) so that `brokkr sidecar dirty`
    /// can identify exactly which run produced the sidecar data.
    ///
    /// When `uuid` is `None` (dirty tree or failed run), generates a fresh
    /// UUID and updates the `dirty` latest pointer so `brokkr results dirty`
    /// always finds the most recent unstored run.
    pub fn store_sidecar(
        &self,
        uuid: Option<&str>,
        sidecar_runs: &[crate::sidecar::SidecarData],
        best_run_idx: usize,
        run_info: Option<&crate::db::sidecar::RunInfo>,
    ) -> Result<(), DevError> {
        let sidecar_db_path = self.db_dir.join("sidecar.db");
        let sidecar_db = crate::db::sidecar::SidecarDb::open(&sidecar_db_path)?;

        let (store_uuid, is_dirty) = match uuid {
            Some(u) => (u.to_owned(), false),
            None => {
                // Generate a unique UUID for this dirty/failed run.
                let mut bytes = [0u8; 16];
                std::fs::File::open("/dev/urandom")
                    .and_then(|mut f| {
                        use std::io::Read;
                        f.read_exact(&mut bytes)
                    })
                    .map_err(DevError::Io)?;
                bytes[6] = (bytes[6] & 0x0f) | 0x40;
                bytes[8] = (bytes[8] & 0x3f) | 0x80;
                let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                (hex, true)
            }
        };

        for (i, data) in sidecar_runs.iter().enumerate() {
            sidecar_db.store_run(&store_uuid, i, data)?;
        }

        sidecar_db.store_meta(&store_uuid, best_run_idx, sidecar_runs.len(), run_info)?;

        if is_dirty {
            sidecar_db.set_latest("dirty", &store_uuid)?;
        }

        let short = &store_uuid[..8.min(store_uuid.len())];
        output::sidecar_msg(&format!("profile data stored in sidecar.db ({short})"));

        // Close the writer before backup. The backup API reads the logical
        // DB state regardless, but closing avoids holding two connections.
        drop(sidecar_db);

        // Back up sidecar DB while the lock is still held.
        if let Err(e) = backup_sidecar(&sidecar_db_path, self.project) {
            output::sidecar_msg(&format!("sidecar backup failed (non-fatal): {e}"));
        }

        Ok(())
    }

    /// Build a `RunRow` from harness state, config, and result.
    fn build_row(&self, config: &BenchConfig, result: &BenchResult) -> RunRow {
        let mut kv = config.metadata.clone();
        // Env capture comes before result kv so runtime counters win on
        // the unlikely key collision (env.foo vs a runtime env.foo).
        kv.extend(self.env_kv.iter().cloned());
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
        let mode = self
            .measure_mode
            .clone()
            .or_else(|| config.mode.clone());
        let brokkr_args = self
            .brokkr_args
            .clone()
            .or_else(|| config.brokkr_args.clone());

        RunRow {
            hostname: self.env.hostname.clone(),
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
            cargo_profile: config.cargo_profile,
            elapsed_ms: result.elapsed_ms,
            kernel: Some(self.env.kernel.clone()),
            cpu_governor: Some(self.env.governor.clone()),
            avail_memory_mb: i64::try_from(self.env.memory_available_mb).ok(),
            storage_notes: self.storage_notes.clone(),
            cli_args: config.cli_args.clone(),
            brokkr_args,
            project: self.project.name().to_owned(),
            stop_marker: self.stop_marker.clone(),
            kv,
            distribution: result.distribution.clone(),
            hotpath: result.hotpath.clone(),
        }
    }
}
