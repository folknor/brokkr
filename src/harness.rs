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
        wait: bool,
        stop_marker: Option<String>,
    ) -> Result<Self, DevError> {
        let lock_ctx = crate::lockfile::LockContext {
            project: project.name(),
            command: lock_command,
            project_root: &project_root.display().to_string(),
        };
        let lock = if wait {
            crate::lockfile::acquire_blocking(&lock_ctx)?
        } else {
            crate::lockfile::acquire(&lock_ctx)?
        };
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
            let child = output::spawn_captured(&prog_str, args, cwd, &env)?;
            last_pid = child.id();
            self.lock.set_child_pid(last_pid);

            // run_sidecar takes ownership of the child, drains stdout/stderr
            // in background threads, and returns everything when the child exits.
            let result = sidecar::run_sidecar(child, &mut fifo, i, start, self.stop_marker.as_deref());
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
            let child = output::spawn_captured(&prog_str, args, cwd, &env)?;
            last_pid = child.id();
            self.lock.set_child_pid(last_pid);
            let sidecar_result = sidecar::run_sidecar(child, &mut fifo, i, start, self.stop_marker.as_deref());
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

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Build a result summary string with key=value pairs.
fn format_result_line(config: &BenchConfig, result: &BenchResult, git: &GitInfo) -> String {
    let mut parts = Vec::with_capacity(8);
    parts.push(format!("command={}", config.command));

    if let Some(ref v) = config.mode {
        parts.push(format!("mode={v}"));
    }

    parts.push(format!("elapsed_ms={}", result.elapsed_ms));
    parts.push(format!("commit={}", git.commit));

    if let Some(ref input) = config.input_file {
        parts.push(format!("input={input}"));
    }

    append_kv_fields(&mut parts, &result.kv);

    // Compute I/O throughput when input size and elapsed time are known.
    if let Some(input_mb) = config.input_mb
        && result.elapsed_ms > 0
    {
        #[allow(clippy::cast_precision_loss)]
        let secs = result.elapsed_ms as f64 / 1000.0;
        let read_mbs = input_mb / secs;
        parts.push(format!("read_mbs={read_mbs:.1}"));
        if let Some(output_bytes) = find_kv_int(&result.kv, "output_bytes") {
            #[allow(clippy::cast_precision_loss)]
            let output_mb = output_bytes as f64 / 1_000_000.0;
            let write_mbs = output_mb / secs;
            parts.push(format!("write_mbs={write_mbs:.1}"));
        }
    }

    if let Some(ref dist) = result.distribution {
        parts.push(format!("samples={}", dist.samples));
        parts.push(format!("min_ms={}", dist.min_ms));
        parts.push(format!("p50_ms={}", dist.p50_ms));
        parts.push(format!("p95_ms={}", dist.p95_ms));
        parts.push(format!("max_ms={}", dist.max_ms));
    }

    parts.join("  ")
}

/// Emit a `[result]` line (respects quiet mode).
fn emit_result_lines(config: &BenchConfig, result: &BenchResult, git: &GitInfo) {
    output::result_msg(&format_result_line(config, result, git));
}

/// Emit a `[result]` line unconditionally (ignores quiet mode).
/// Used for dirty-tree results that can't be looked up later.
fn force_emit_result_lines(config: &BenchConfig, result: &BenchResult, git: &GitInfo) {
    println!("[result]  {}", format_result_line(config, result, git));
}

/// Look up an integer KV pair by key.
fn find_kv_int(kv: &[KvPair], key: &str) -> Option<i64> {
    kv.iter()
        .find(|p| p.key == key)
        .and_then(|p| match &p.value {
            KvValue::Int(v) => Some(*v),
            _ => None,
        })
}

/// Flatten key-value pairs into the result line.
fn append_kv_fields(parts: &mut Vec<String>, kv: &[KvPair]) {
    for pair in kv {
        parts.push(format!("{}={}", pair.key, pair.value));
    }
}

/// Extract an exit code from a process `ExitStatus`.
///
/// Returns the exit code if the process exited normally, or `128 + signal`
/// if it was killed by a signal (matching shell convention: 137 = OOM kill).
fn clamp_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn exit_code_from_status(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    // Killed by signal - use shell convention (128 + signum).
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    -1
}

/// Current wall-clock time as seconds since the Unix epoch.
fn wall_clock_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// Format a program path and argument slice into a single command-line string.
///
/// Quotes arguments that contain spaces. Used to populate `BenchConfig.cli_args`.
pub fn format_cli_args(program: &str, args: &[&str]) -> String {
    let mut parts = Vec::with_capacity(1 + args.len());
    parts.push(maybe_quote(program));
    for arg in args {
        parts.push(maybe_quote(arg));
    }
    parts.join(" ")
}

/// Run a closure for each variant, collecting failures instead of aborting.
///
/// Each variant runs independently - failure of one does not skip the rest.
/// On completion, returns `Ok(())` if all succeeded, or a summary error
/// listing which variants failed and why.
///
/// Usage:
/// ```ignore
/// run_variants("mode", &["sequential", "parallel", "pipelined"], |variant| {
///     // set up config using variant name...
///     harness.run_external(&config, binary, &args, project_root)
/// })?;
/// ```
pub fn run_variants<F>(label: &str, variants: &[&str], mut run_one: F) -> Result<(), DevError>
where
    F: FnMut(&str) -> Result<(), DevError>,
{
    let mut failures: Vec<(&str, String)> = Vec::new();

    for &variant in variants {
        output::bench_msg(&format!("{label}: {variant}"));
        if let Err(e) = run_one(variant) {
            output::error(&format!("{variant} failed: {e}"));
            failures.push((variant, e.to_string()));
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        let summary: Vec<String> = failures
            .iter()
            .map(|(v, e)| format!("{v}: {e}"))
            .collect();
        Err(DevError::Verify(format!(
            "{} of {} variants failed:\n  {}",
            failures.len(),
            variants.len(),
            summary.join("\n  "),
        )))
    }
}

fn maybe_quote(s: &str) -> String {
    if s.contains(' ') {
        format!("\"{s}\"")
    } else {
        s.to_owned()
    }
}

/// Return the cargo feature name for hotpath mode: `"hotpath"` or `"hotpath-alloc"`.
pub fn hotpath_feature(alloc: bool) -> &'static str {
    if alloc { "hotpath-alloc" } else { "hotpath" }
}

/// Convert a `Duration` to milliseconds as `i64`.
pub fn elapsed_to_ms(duration: &Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// Run a binary with hotpath env vars and sidecar monitoring, capture the
/// JSON report, and return a `BenchResult` plus sidecar data.
///
/// Sets `HOTPATH_METRICS_SERVER_OFF`, `HOTPATH_OUTPUT_FORMAT`,
/// `HOTPATH_OUTPUT_PATH`, and `BROKKR_MARKER_FIFO`. Uses `spawn_captured`
/// + sidecar loop so /proc metrics are sampled during the run.
///
/// Creates and manages its own FIFO in `scratch_dir`.
#[allow(clippy::too_many_arguments)]
pub fn run_hotpath_capture(
    binary: &str,
    args: &[&str],
    scratch_dir: &std::path::Path,
    project_root: &std::path::Path,
    extra_env: &[(&str, &str)],
    ok_codes: &[i32],
    stop_marker: Option<&str>,
    lock: Option<&LockGuard>,
) -> Result<(BenchResult, Vec<u8>, crate::sidecar::SidecarData), crate::error::DevError> {
    let json_file = scratch_dir.join("hotpath-report.json");
    let json_file_str = json_file.display().to_string();

    let mut fifo = crate::sidecar::SidecarFifo::create(scratch_dir)?;
    let fifo_path_str = fifo.path_str()?.to_owned();

    let mut env: Vec<(&str, &str)> = vec![
        ("HOTPATH_METRICS_SERVER_OFF", "true"),
        ("HOTPATH_OUTPUT_FORMAT", "json"),
        ("HOTPATH_OUTPUT_PATH", &json_file_str),
        ("BROKKR_MARKER_FIFO", &fifo_path_str),
    ];
    env.extend_from_slice(extra_env);

    let start = std::time::Instant::now();
    let child = output::spawn_captured(binary, args, project_root, &env)?;
    if let Some(lock) = lock {
        lock.set_child_pid(child.id());
    }
    let sidecar_result = crate::sidecar::run_sidecar(child, &mut fifo, 0, start, stop_marker);
    let stopped = sidecar_result.stopped_by_marker;
    let interrupted = sidecar_result.stopped_by_signal;

    drop(fifo);

    let captured = output::CapturedOutput {
        status: sidecar_result.exit_status,
        stdout: sidecar_result.stdout,
        stderr: sidecar_result.stderr,
        elapsed: sidecar_result.elapsed,
    };
    if interrupted {
        return Err(crate::error::DevError::Interrupted);
    }
    if !stopped {
        captured.check_success_or(binary, ok_codes)?;
    }

    let ms = elapsed_to_ms(&captured.elapsed);
    let (_stderr_ms, kv) = parse_kv_lines(&captured.stderr);
    let stderr = captured.stderr;

    let hotpath = match std::fs::read_to_string(&json_file) {
        Ok(s) => match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(v) => db::hotpath_data_from_json(&v),
            Err(e) => {
                output::error(&format!("failed to parse hotpath JSON: {e}"));
                None
            }
        },
        Err(e) => {
            output::error(&format!(
                "failed to read hotpath report {}: {e}",
                json_file.display()
            ));
            None
        }
    };
    std::fs::remove_file(&json_file).ok();

    Ok((
        BenchResult {
            elapsed_ms: ms,
            kv,
            distribution: None,
            hotpath,
        },
        stderr,
        sidecar_result.data,
    ))
}

/// Compute a percentile from a sorted slice using linear interpolation.
///
/// Uses the "C = 1" variant (linear interpolation between adjacent ranks).
/// This avoids the systematic underestimation of high percentiles (e.g. p95)
/// that nearest-rank with integer truncation produces on small sample counts.
fn percentile(sorted: &[i64], pct: usize) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let len = sorted.len();
    if len == 1 {
        return sorted[0];
    }
    // Fractional index into the sorted array.
    #[allow(clippy::cast_precision_loss)]
    let pos = (pct as f64 / 100.0) * (len - 1) as f64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lo = pos as usize;
    let hi = (lo + 1).min(len - 1);
    #[allow(clippy::cast_precision_loss)]
    let frac = pos - lo as f64;
    // Linear interpolation: sorted[lo] + frac * (sorted[hi] - sorted[lo])
    #[allow(clippy::cast_precision_loss)]
    let result = sorted[lo] as f64 + frac * (sorted[hi] - sorted[lo]) as f64;
    #[allow(clippy::cast_possible_truncation)]
    {
        result.round() as i64
    }
}

/// Pick the `BenchResult` with the smaller `elapsed_ms`.
fn pick_best(current: Option<BenchResult>, candidate: BenchResult) -> BenchResult {
    match current {
        Some(best) if best.elapsed_ms <= candidate.elapsed_ms => best,
        _ => candidate,
    }
}

/// Build a storage notes string from the drive configuration.
fn format_storage_notes(drives: &Option<DriveConfig>) -> Option<String> {
    let drives = drives.as_ref()?;

    let mut parts = Vec::with_capacity(4);
    push_drive_note(&mut parts, "source", &drives.source);
    push_drive_note(&mut parts, "data", &drives.data);
    push_drive_note(&mut parts, "scratch", &drives.scratch);
    push_drive_note(&mut parts, "target", &drives.target);

    if parts.is_empty() {
        return None;
    }

    Some(parts.join(", "))
}

/// Append a "label=value" note if the drive field is present.
fn push_drive_note(parts: &mut Vec<String>, label: &str, value: &Option<String>) {
    if let Some(v) = value {
        parts.push(format!("{label}={v}"));
    }
}

/// Parse stderr bytes for `key=value` lines. Extracts `elapsed_ms` for timing,
/// puts all other kv pairs into `BenchResult.kv`.
/// Parse `key=value` lines from stderr, returning `(elapsed_ms, kv_pairs)`.
/// `elapsed_ms` is `None` when no `elapsed_ms`/`total_ms` line is found.
pub(crate) fn parse_kv_lines(stderr: &[u8]) -> (Option<i64>, Vec<KvPair>) {
    let text = String::from_utf8_lossy(stderr);
    let mut elapsed_ms: Option<i64> = None;
    let mut kv = Vec::new();

    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if key == "elapsed_ms" || key == "total_ms" {
                if let Ok(ms) = value.parse::<i64>() {
                    elapsed_ms = Some(ms);
                }
            } else if let Ok(n) = value.parse::<i64>() {
                kv.push(KvPair::int(key, n));
            } else if let Ok(f) = value.parse::<f64>() {
                if f.is_finite() {
                    kv.push(KvPair::real(key, f));
                } else {
                    kv.push(KvPair::text(key, value));
                }
            } else {
                kv.push(KvPair::text(key, value));
            }
        }
    }

    (elapsed_ms, kv)
}

fn parse_kv_stderr(stderr: &[u8]) -> Result<BenchResult, DevError> {
    let (elapsed_ms, kv) = parse_kv_lines(stderr);

    let elapsed_ms = elapsed_ms.ok_or_else(|| {
        let text = String::from_utf8_lossy(stderr);
        let preview: String = text.chars().take(500).collect();
        DevError::Config(format!(
            "subprocess stderr missing elapsed_ms=NNN. stderr was:\n{preview}"
        ))
    })?;

    Ok(BenchResult {
        elapsed_ms,
        kv,
        distribution: None,
        hotpath: None,
    })
}

// ---------------------------------------------------------------------------
// Sidecar backup
// ---------------------------------------------------------------------------

/// Number of rotating backup copies to keep.
const SIDECAR_BACKUP_COPIES: usize = 3;

/// Resolve the sidecar backup directory.
///
/// Uses `$XDG_DATA_HOME/brokkr/sidecar-backups/`, falling back to
/// `$HOME/.local/share/brokkr/sidecar-backups/`.
fn sidecar_backup_dir() -> Result<PathBuf, DevError> {
    let data_dir = if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local").join("share")
    } else {
        return Err(DevError::Config(
            "cannot determine data directory for sidecar backup".into(),
        ));
    };
    Ok(data_dir.join("brokkr").join("sidecar-backups"))
}

/// Back up the sidecar DB with rotation and fsync.
///
/// Keeps `SIDECAR_BACKUP_COPIES` versions:
///   {project}-sidecar.db      (newest)
///   {project}-sidecar.db.1    (previous)
///   {project}-sidecar.db.2    (oldest)
///
/// Uses SQLite's online backup API to produce a self-contained backup
/// (DELETE journal mode, no WAL side files). The backup captures the
/// logical database state regardless of WAL or concurrent readers.
///
/// The sequence is: create temp backup via SQLite → quick_check the
/// temp → fsync → shift older copies → hard-link primary to .1 →
/// atomic rename temp into primary slot → fsync directory. The primary
/// slot is only overwritten by the atomic rename, so a failure before
/// that point leaves the current primary intact.
///
/// Called while the benchmark lock is still held.
fn backup_sidecar(
    sidecar_path: &Path,
    project: crate::project::Project,
) -> Result<(), DevError> {
    backup_sidecar_to(sidecar_path, project, None)
}

/// Inner implementation that accepts an optional backup directory override
/// (used by tests to avoid mutating global XDG_DATA_HOME).
fn backup_sidecar_to(
    sidecar_path: &Path,
    project: crate::project::Project,
    backup_dir_override: Option<&Path>,
) -> Result<(), DevError> {
    if !sidecar_path.exists() {
        return Ok(());
    }

    let backup_dir = match backup_dir_override {
        Some(d) => d.to_path_buf(),
        None => sidecar_backup_dir()?,
    };
    std::fs::create_dir_all(&backup_dir)?;

    let base = backup_dir.join(format!("{}-sidecar.db", project.name()));
    let tmp = base.with_extension("db.tmp");

    // Clean up any stale tmp from a previous interrupted run.
    if tmp.exists() {
        std::fs::remove_file(&tmp).ok();
    }

    // Create backup via SQLite backup API. This reads the logical DB state
    // (including uncommitted WAL pages from other connections) and writes a
    // self-contained DELETE-journal-mode database at the temp path. The
    // backup API also runs quick_check on the result.
    if let Err(e) = crate::db::sidecar::backup_to_path(sidecar_path, &tmp) {
        // Clean up failed temp on best effort.
        std::fs::remove_file(&tmp).ok();
        return Err(e);
    }

    // fsync the temp backup before rotating.
    let file = std::fs::File::open(&tmp)?;
    file.sync_all()?;
    drop(file);

    // Promote the new backup into the primary slot without displacing the
    // current primary until the new one is in place.
    //
    // Sequence:
    //   1. Shift older copies: .1 → .2 (clears .1 slot, drops oldest)
    //   2. Preserve current primary: hard-link base → .1
    //   3. Atomic promote: rename tmp → base (overwrites old base)
    //
    // Every step propagates errors. If any rotation or preservation step
    // fails, the backup is considered failed rather than silently losing
    // retention history.

    // Shift older copies: .1 → .2, .2 → .3, etc.
    // This clears the .1 slot so the hard-link in the next step can
    // succeed without a prior remove.
    for i in (2..SIDECAR_BACKUP_COPIES).rev() {
        let from = base.with_extension(format!("db.{}", i - 1));
        let to = base.with_extension(format!("db.{i}"));
        if from.exists() {
            std::fs::rename(&from, &to)?;
        }
    }

    // Preserve the current primary as .1 via hard-link.
    // The .1 slot was cleared by the rename above (or never existed).
    if base.exists() {
        let slot1 = base.with_extension("db.1");
        std::fs::hard_link(&base, &slot1)?;
    }

    // Atomic promotion: rename tmp → base. On Linux this atomically
    // replaces the old base. If this fails, base is still the old copy
    // (the hard-link in step 2 created .1 as a second link to the same
    // inode, so the data is preserved regardless).
    std::fs::rename(&tmp, &base)?;

    // fsync the directory to make all renames durable.
    let dir = std::fs::File::open(&backup_dir)?;
    dir.sync_all()?;

    output::sidecar_msg(&format!("backup: {}", base.display()));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
    use super::*;

    // -----------------------------------------------------------------------
    // percentile
    // -----------------------------------------------------------------------

    #[test]
    fn percentile_empty_returns_zero() {
        assert_eq!(percentile(&[], 50), 0, "empty slice should return 0");
    }

    #[test]
    fn percentile_single_element_ignores_pct() {
        assert_eq!(percentile(&[42], 0), 42);
        assert_eq!(percentile(&[42], 50), 42);
        assert_eq!(percentile(&[42], 100), 42);
    }

    #[test]
    fn percentile_two_elements_interpolates() {
        // [100, 200]: p0=100, p50=150, p100=200
        let data = vec![100, 200];
        assert_eq!(percentile(&data, 0), 100);
        assert_eq!(
            percentile(&data, 50),
            150,
            "midpoint should interpolate to 150"
        );
        assert_eq!(percentile(&data, 100), 200);
        // p25 = 100 + 0.25*(200-100) = 125
        assert_eq!(percentile(&data, 25), 125);
        // p75 = 100 + 0.75*(200-100) = 175
        assert_eq!(percentile(&data, 75), 175);
    }

    #[test]
    fn percentile_five_elements_at_boundaries() {
        let data = vec![10, 20, 30, 40, 50];
        assert_eq!(percentile(&data, 0), 10);
        assert_eq!(
            percentile(&data, 25),
            20,
            "p25 of [10,20,30,40,50] should be 20"
        );
        assert_eq!(percentile(&data, 50), 30, "p50 should be median");
        assert_eq!(percentile(&data, 75), 40);
        assert_eq!(percentile(&data, 100), 50);
    }

    #[test]
    fn percentile_interpolation_beats_nearest_rank() {
        // With 3 samples [0, 100, 1000], nearest-rank p95 would pick index 2 = 1000.
        // Linear interpolation: pos = 0.95 * 2 = 1.9, lo=1(100), hi=2(1000)
        // result = 100 + 0.9 * 900 = 910
        let data = vec![0, 100, 1000];
        let p95 = percentile(&data, 95);
        assert_eq!(
            p95, 910,
            "linear interpolation should yield 910, not nearest-rank 1000"
        );
        assert!(
            p95 < 1000,
            "interpolated p95 must be less than max for non-degenerate data"
        );
    }

    #[test]
    fn percentile_identical_values() {
        let data = vec![7, 7, 7, 7];
        assert_eq!(percentile(&data, 0), 7);
        assert_eq!(percentile(&data, 50), 7);
        assert_eq!(percentile(&data, 100), 7);
    }

    // -----------------------------------------------------------------------
    // parse_kv_stderr
    // -----------------------------------------------------------------------

    #[test]
    fn parse_kv_stderr_basic_elapsed_ms() {
        let stderr = b"elapsed_ms=1234\n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(result.elapsed_ms, 1234);
        assert!(
            result.kv.is_empty(),
            "no extra fields => kv should be empty"
        );
    }

    #[test]
    fn parse_kv_stderr_total_ms_alias() {
        let stderr = b"total_ms=999\n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(
            result.elapsed_ms, 999,
            "total_ms should be accepted as elapsed_ms alias"
        );
    }

    #[test]
    fn parse_kv_stderr_elapsed_ms_takes_precedence_over_total_ms() {
        // Both present: last one wins (due to overwrite semantics)
        let stderr = b"total_ms=100\nelapsed_ms=200\n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(
            result.elapsed_ms, 200,
            "elapsed_ms should overwrite earlier total_ms"
        );

        // Reverse order: total_ms overwrites elapsed_ms
        let stderr2 = b"elapsed_ms=200\ntotal_ms=100\n";
        let result2 = parse_kv_stderr(stderr2).unwrap();
        assert_eq!(result2.elapsed_ms, 100, "last key=value wins");
    }

    #[test]
    fn parse_kv_stderr_extra_int_float_string_fields() {
        let stderr = b"elapsed_ms=500\nrows=42\nrate=3.14\nlabel=fast\n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(result.elapsed_ms, 500);

        assert_eq!(result.kv.len(), 3);
        // Check that we have the expected keys and values
        let find = |k: &str| result.kv.iter().find(|p| p.key == k).unwrap();
        assert!(matches!(find("rows").value, KvValue::Int(42)));
        assert!(matches!(find("rate").value, KvValue::Real(r) if (r - 3.14).abs() < 0.001));
        assert!(matches!(&find("label").value, KvValue::Text(s) if s == "fast"));
    }

    #[test]
    fn parse_kv_stderr_missing_elapsed_ms_error() {
        let stderr = b"rows=100\nlabel=test\n";
        match parse_kv_stderr(stderr) {
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("missing elapsed_ms"),
                    "error should mention missing elapsed_ms, got: {msg}"
                );
            }
            Ok(_) => panic!("expected error for missing elapsed_ms, got Ok"),
        }
    }

    #[test]
    fn parse_kv_stderr_mixed_garbage_lines() {
        let stderr = b"some random log output\nwarning: something\nelapsed_ms=777\nmore junk\n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(
            result.elapsed_ms, 777,
            "should find elapsed_ms among garbage lines"
        );
    }

    #[test]
    fn parse_kv_stderr_empty_value_treated_as_string() {
        // "tag=" has empty value - not parseable as i64 or f64, so becomes a string
        let stderr = b"elapsed_ms=100\ntag=\n";
        let result = parse_kv_stderr(stderr).unwrap();
        let find = |k: &str| result.kv.iter().find(|p| p.key == k).unwrap();
        assert!(
            matches!(&find("tag").value, KvValue::Text(s) if s.is_empty()),
            "empty value should become empty string"
        );
    }

    #[test]
    fn parse_kv_stderr_whitespace_trimming() {
        let stderr = b"  elapsed_ms  =  300  \n  count  =  5  \n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(result.elapsed_ms, 300, "keys and values should be trimmed");
        let find = |k: &str| result.kv.iter().find(|p| p.key == k).unwrap();
        assert!(matches!(find("count").value, KvValue::Int(5)));
    }

    #[test]
    fn parse_kv_stderr_invalid_elapsed_ms_value() {
        let stderr = b"elapsed_ms=not_a_number\n";
        match parse_kv_stderr(stderr) {
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("missing elapsed_ms"),
                    "should report missing elapsed_ms for unparseable value, got: {msg}"
                );
            }
            Ok(_) => panic!("expected error for invalid elapsed_ms value, got Ok"),
        }
    }

    #[test]
    fn parse_kv_stderr_nan_float_becomes_string() {
        // NaN is not representable in JSON numbers
        let stderr = b"elapsed_ms=100\nweird=NaN\n";
        let result = parse_kv_stderr(stderr).unwrap();
        let find = |k: &str| result.kv.iter().find(|p| p.key == k).unwrap();
        assert!(
            matches!(&find("weird").value, KvValue::Text(s) if s == "NaN"),
            "NaN should fall through to string"
        );
    }

    // -----------------------------------------------------------------------
    // format_cli_args / maybe_quote
    // -----------------------------------------------------------------------

    #[test]
    fn format_cli_args_no_args() {
        assert_eq!(format_cli_args("./bench", &[]), "./bench");
    }

    #[test]
    fn format_cli_args_simple_args() {
        assert_eq!(
            format_cli_args("./bench", &["--fast", "-n", "10"]),
            "./bench --fast -n 10"
        );
    }

    #[test]
    fn format_cli_args_args_with_spaces_get_quoted() {
        assert_eq!(
            format_cli_args("./my tool", &["--input", "path with spaces", "--verbose"]),
            "\"./my tool\" --input \"path with spaces\" --verbose"
        );
    }

    #[test]
    fn maybe_quote_no_spaces() {
        assert_eq!(maybe_quote("simple"), "simple");
    }

    #[test]
    fn maybe_quote_with_spaces() {
        assert_eq!(maybe_quote("has space"), "\"has space\"");
    }

    #[test]
    fn maybe_quote_empty_string() {
        assert_eq!(
            maybe_quote(""),
            "",
            "empty string has no spaces, should not be quoted"
        );
    }

    // -----------------------------------------------------------------------
    // pick_best / pick_best_ms
    // -----------------------------------------------------------------------

    #[test]
    fn pick_best_none_vs_candidate() {
        let candidate = BenchResult {
            elapsed_ms: 500,
            kv: vec![],
            distribution: None,
            hotpath: None,
        };
        let result = pick_best(None, candidate);
        assert_eq!(
            result.elapsed_ms, 500,
            "None current should always take candidate"
        );
    }

    #[test]
    fn pick_best_keeps_better() {
        let current = BenchResult {
            elapsed_ms: 100,
            kv: vec![],
            distribution: None,
            hotpath: None,
        };
        let candidate = BenchResult {
            elapsed_ms: 200,
            kv: vec![],
            distribution: None,
            hotpath: None,
        };
        let result = pick_best(Some(current), candidate);
        assert_eq!(result.elapsed_ms, 100, "should keep the lower value");
    }

    #[test]
    fn pick_best_replaces_with_better() {
        let current = BenchResult {
            elapsed_ms: 300,
            kv: vec![],
            distribution: None,
            hotpath: None,
        };
        let candidate = BenchResult {
            elapsed_ms: 150,
            kv: vec![],
            distribution: None,
            hotpath: None,
        };
        let result = pick_best(Some(current), candidate);
        assert_eq!(result.elapsed_ms, 150, "should replace with lower value");
    }

    #[test]
    fn pick_best_equal_keeps_current() {
        // Tie-breaking: current wins (<=)
        let current = BenchResult {
            elapsed_ms: 100,
            kv: vec![KvPair::text("tag", "first")],
            distribution: None,
            hotpath: None,
        };
        let candidate = BenchResult {
            elapsed_ms: 100,
            kv: vec![KvPair::text("tag", "second")],
            distribution: None,
            hotpath: None,
        };
        let result = pick_best(Some(current), candidate);
        let tag = result.kv.iter().find(|p| p.key == "tag").unwrap();
        assert!(
            matches!(&tag.value, KvValue::Text(s) if s == "first"),
            "on tie, current (first seen) should be kept"
        );
    }

    // -----------------------------------------------------------------------
    // elapsed_to_ms
    // -----------------------------------------------------------------------

    #[test]
    fn elapsed_to_ms_normal() {
        let d = Duration::from_millis(1234);
        assert_eq!(elapsed_to_ms(&d), 1234);
    }

    #[test]
    fn elapsed_to_ms_zero() {
        let d = Duration::ZERO;
        assert_eq!(elapsed_to_ms(&d), 0);
    }

    #[test]
    fn elapsed_to_ms_overflow_saturates() {
        // Duration can hold values larger than i64::MAX milliseconds.
        // u64::MAX seconds = ~584 billion years worth of milliseconds, way beyond i64::MAX.
        let d = Duration::from_secs(u64::MAX);
        assert_eq!(
            elapsed_to_ms(&d),
            i64::MAX,
            "overflow should saturate to i64::MAX"
        );
    }

    #[test]
    fn elapsed_to_ms_sub_millisecond_truncates() {
        let d = Duration::from_micros(999);
        assert_eq!(elapsed_to_ms(&d), 0, "sub-millisecond should truncate to 0");
    }

    // -----------------------------------------------------------------------
    // hotpath_feature
    // -----------------------------------------------------------------------

    #[test]
    fn hotpath_feature_without_alloc() {
        assert_eq!(hotpath_feature(false), "hotpath");
    }

    #[test]
    fn hotpath_feature_with_alloc() {
        assert_eq!(hotpath_feature(true), "hotpath-alloc");
    }

    // -------------------------------------------------------------------
    // backup_sidecar rotation
    // -------------------------------------------------------------------

    fn temp_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brokkr-harness-test-{}-{}",
            std::process::id(),
            suffix,
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Create a minimal sidecar DB at the given path.
    fn create_sidecar(path: &Path) {
        let db = crate::db::sidecar::SidecarDb::open(path).unwrap();
        db.conn().execute(
            "INSERT INTO sidecar_markers (result_uuid, run_idx, marker_idx, \
             timestamp_us, name) VALUES ('test', 0, 0, 1000, 'marker')",
            [],
        ).unwrap();
    }

    #[test]
    fn backup_sidecar_creates_and_rotates() {
        let dir = temp_dir("rotate");
        let sidecar_path = dir.join("sidecar.db");
        create_sidecar(&sidecar_path);

        let backup_dir = dir.join("backups");

        // Run backup 4 times to exercise rotation.
        for _ in 0..4 {
            backup_sidecar_to(
                &sidecar_path,
                crate::project::Project::Pbfhogg,
                Some(&backup_dir),
            )
            .unwrap();
        }

        let base = backup_dir.join("pbfhogg-sidecar.db");
        assert!(base.exists(), "newest backup should exist");
        assert!(
            base.with_extension("db.1").exists(),
            "second backup should exist"
        );
        assert!(
            base.with_extension("db.2").exists(),
            "third backup should exist"
        );
        // Only 3 copies kept.
        assert!(
            !base.with_extension("db.3").exists(),
            "fourth backup should not exist"
        );

        // Verify the backup is a valid SQLite DB.
        let conn = rusqlite::Connection::open_with_flags(
            &base,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sidecar_markers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backup_failure_does_not_displace_good_backup() {
        let dir = temp_dir("nodisplace");
        let sidecar_path = dir.join("sidecar.db");
        create_sidecar(&sidecar_path);

        let backup_dir = dir.join("backups");

        // Create a good initial backup.
        backup_sidecar_to(
            &sidecar_path,
            crate::project::Project::Pbfhogg,
            Some(&backup_dir),
        )
        .unwrap();

        let base = backup_dir.join("pbfhogg-sidecar.db");
        assert!(base.exists());
        let good_size = std::fs::metadata(&base).unwrap().len();

        // Attempt backup from a non-SQLite source - should fail.
        let bad_source = dir.join("not-a-database.db");
        std::fs::write(&bad_source, b"this is not sqlite").unwrap();

        let result = backup_sidecar_to(
            &bad_source,
            crate::project::Project::Pbfhogg,
            Some(&backup_dir),
        );
        assert!(result.is_err());

        // The good backup should still be intact.
        assert!(base.exists(), "good backup should still exist");
        let after_size = std::fs::metadata(&base).unwrap().len();
        assert_eq!(good_size, after_size, "good backup should be unchanged");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backup_sidecar_nonexistent_source_is_noop() {
        let dir = temp_dir("noop");
        let sidecar_path = dir.join("does-not-exist.db");

        let result = backup_sidecar_to(
            &sidecar_path,
            crate::project::Project::Pbfhogg,
            Some(&dir),
        );
        assert!(result.is_ok());

        std::fs::remove_dir_all(&dir).ok();
    }
}
