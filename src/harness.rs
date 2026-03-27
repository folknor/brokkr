use std::path::Path;
use std::time::{Duration, Instant};

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
    pub variant: Option<String>,
    pub input_file: Option<String>,
    pub input_mb: Option<f64>,
    pub cargo_features: Option<String>,
    pub cargo_profile: String,
    pub runs: usize,
    pub cli_args: Option<String>,
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
    _lock: LockGuard,
    db: ResultsDb,
    env: EnvInfo,
    git: GitInfo,
    storage_notes: Option<String>,
    cargo_features: Option<String>,
    project: crate::project::Project,
}

impl BenchHarness {
    /// Create a new harness, acquiring the lockfile and collecting environment.
    ///
    /// When `db_root` is `Some`, the results DB is opened from that directory
    /// instead of `project_root`. This is used for worktree-based benchmarking
    /// where git info comes from the worktree but results are stored in the
    /// main tree's database.
    pub fn new(
        paths: &crate::config::ResolvedPaths,
        project_root: &Path,
        db_root: Option<&Path>,
        project: crate::project::Project,
        lock_command: &str,
        force: bool,
    ) -> Result<Self, DevError> {
        let lock = crate::lockfile::acquire(&crate::lockfile::LockContext {
            project: project.name(),
            command: lock_command,
            project_root: &project_root.display().to_string(),
        })?;
        Self::new_with_lock(lock, paths, project_root, db_root, project, force)
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
                output::error("WARNING: dirty tree — results will NOT be stored in database");
            } else {
                return Err(DevError::Preflight(vec![
                    "dirty tree — commit or stash changes before benchmarking".into(),
                    "run with --force to bench anyway (results will not be stored)".into(),
                ]));
            }
        }

        Ok(Self {
            _lock: lock,
            db,
            env,
            git,
            storage_notes,
            cargo_features: None,
            project,
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

    /// Internal timing: closure called N times, returns `BenchResult`.
    /// Best-of-N (minimum `elapsed_ms`).
    pub fn run_internal<F>(&self, config: &BenchConfig, f: F) -> Result<BenchResult, DevError>
    where
        F: Fn(usize) -> Result<BenchResult, DevError>,
    {
        let mut best: Option<BenchResult> = None;

        for i in 0..config.runs {
            output::bench_msg(&format!("run {}/{}", i + 1, config.runs));
            let result = f(i)?;
            best = Some(pick_best(best, result));
        }

        let best =
            best.ok_or_else(|| DevError::Config("benchmark requires at least 1 run".into()))?;

        self.record_result(config, &best)?;
        Ok(best)
    }

    /// External timing: run subprocess N times, measure wall-clock.
    /// Best-of-N (minimum).
    pub fn run_external(
        &self,
        config: &BenchConfig,
        program: &Path,
        args: &[&str],
        cwd: &Path,
    ) -> Result<BenchResult, DevError> {
        let mut best_ms: Option<i64> = None;

        for i in 0..config.runs {
            output::bench_msg(&format!("run {}/{}", i + 1, config.runs));

            let start = Instant::now();
            let captured = output::run_captured(&program.display().to_string(), args, cwd)?;
            let ms = elapsed_to_ms(&start.elapsed());

            captured.check_success(&program.display().to_string())?;

            best_ms = Some(pick_best_ms(best_ms, ms));
        }

        let elapsed_ms =
            best_ms.ok_or_else(|| DevError::Config("benchmark requires at least 1 run".into()))?;

        let result = BenchResult {
            elapsed_ms,
            kv: Vec::new(),
            distribution: None,
            hotpath: None,
        };
        self.record_result(config, &result)?;
        Ok(result)
    }

    /// External timing with sidecar: run subprocess N times with a monitoring
    /// sidecar attached. The sidecar samples `/proc` metrics and reads phase
    /// markers from a FIFO. Sidecar data is stored in the results DB alongside
    /// the benchmark result.
    ///
    /// The sidecar takes ownership of each `Child` process, drains
    /// stdout/stderr in background threads (preventing pipe-buffer deadlock),
    /// and records the child's exact exit time for wall-clock measurement.
    pub fn run_external_with_sidecar(
        &self,
        config: &BenchConfig,
        program: &Path,
        args: &[&str],
        cwd: &Path,
        scratch_dir: &Path,
    ) -> Result<BenchResult, DevError> {
        use crate::sidecar;

        let mut fifo = sidecar::SidecarFifo::create(scratch_dir)?;
        let fifo_path_str = fifo.path_str()?.to_owned();

        let mut best_ms: Option<i64> = None;
        let mut best_run_idx: usize = 0;
        let mut sidecar_runs: Vec<sidecar::SidecarData> = Vec::with_capacity(config.runs);
        let prog_str = program.display().to_string();

        for i in 0..config.runs {
            output::bench_msg(&format!("run {}/{}", i + 1, config.runs));

            // Reopen FIFO read end between runs so the next child's write
            // end connects to a fresh reader (not one stuck at EOF).
            if i > 0 {
                fifo.reopen()?;
            }

            let env = [("BROKKR_MARKER_FIFO", fifo_path_str.as_str())];
            let start = Instant::now();
            let child = output::spawn_captured(&prog_str, args, cwd, &env)?;

            // run_sidecar takes ownership of the child, drains stdout/stderr
            // in background threads, and returns everything when the child exits.
            let result = sidecar::run_sidecar(child, &mut fifo, i, start);

            // Check exit status using captured stderr for error messages.
            let captured = output::CapturedOutput {
                status: result.exit_status,
                stdout: result.stdout,
                stderr: result.stderr,
                elapsed: result.elapsed,
            };
            let ms = elapsed_to_ms(&captured.elapsed);
            captured.check_success(&prog_str)?;

            if best_ms.is_none() || ms < best_ms.unwrap() {
                best_ms = Some(ms);
                best_run_idx = i;
            }
            sidecar_runs.push(result.data);
        }

        // FIFO cleaned up by Drop (also handles error paths).
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

        // Store sidecar data if we got a UUID (clean tree).
        if let Some(ref uuid) = uuid {
            for (i, data) in sidecar_runs.iter().enumerate() {
                self.db.store_sidecar_run(uuid, i, data)?;
            }
            output::sidecar_msg(&format!(
                "profile data stored in results.db (best run: {best_run_idx})",
            ));
        }

        Ok(bench_result)
    }

    /// Distribution timing: collect all N samples, compute min/p50/p95/max.
    pub fn run_distribution<F>(&self, config: &BenchConfig, f: F) -> Result<BenchResult, DevError>
    where
        F: Fn(usize) -> Result<i64, DevError>,
    {
        let mut samples = Vec::with_capacity(config.runs);

        for i in 0..config.runs {
            output::bench_msg(&format!("run {}/{}", i + 1, config.runs));
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

    /// External timing with kv parsing: run subprocess N times, parse stderr for key=value lines.
    /// Uses the subprocess's self-reported `elapsed_ms` from stderr (not external wall-clock).
    /// Best-of-N (minimum elapsed_ms).
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

    /// Like `run_external_with_kv` but does NOT record — returns the best
    /// result and the raw stderr from the best run. Caller is responsible for
    /// calling `record_result` after any post-processing.
    pub fn run_external_with_kv_raw(
        &self,
        config: &BenchConfig,
        program: &Path,
        args: &[&str],
        cwd: &Path,
    ) -> Result<(BenchResult, Vec<u8>), DevError> {
        let mut best: Option<BenchResult> = None;
        let mut best_stderr: Vec<u8> = Vec::new();

        for i in 0..config.runs {
            output::bench_msg(&format!("run {}/{}", i + 1, config.runs));

            let captured = output::run_captured(&program.display().to_string(), args, cwd)?;

            captured.check_success(&program.display().to_string())?;

            let result = parse_kv_stderr(&captured.stderr)?;
            let is_new_best = best
                .as_ref()
                .is_none_or(|b| result.elapsed_ms < b.elapsed_ms);
            if is_new_best {
                best_stderr = captured.stderr;
            }
            best = Some(pick_best(best, result));
        }

        let best =
            best.ok_or_else(|| DevError::Config("benchmark requires at least 1 run".into()))?;

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
            let (uuid, short) = self.db.insert_full(&row)?;
            emit_result_lines(config, result, &self.git);
            output::bench_msg(&format!("stored in results.db ({short})"));
            println!("{short}");
            Ok(Some(uuid))
        } else {
            // Dirty tree: no DB insert, no UUID. Always print result line
            // since the data can't be looked up later.
            force_emit_result_lines(config, result, &self.git);
            output::error("NOT STORED — tree is dirty (commit or stash changes)");
            Ok(None)
        }
    }

    /// Build a `RunRow` from harness state, config, and result.
    fn build_row(&self, config: &BenchConfig, result: &BenchResult) -> RunRow {
        let mut kv = config.metadata.clone();
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

        RunRow {
            hostname: self.env.hostname.clone(),
            commit: self.git.commit.clone(),
            subject: self.git.subject.clone(),
            command: config.command.clone(),
            variant: config.variant.clone(),
            input_file: config.input_file.clone(),
            input_mb: config.input_mb,
            peak_rss_mb,
            cargo_features: config
                .cargo_features
                .clone()
                .or_else(|| self.cargo_features.clone()),
            cargo_profile: config.cargo_profile.clone(),
            elapsed_ms: result.elapsed_ms,
            kernel: Some(self.env.kernel.clone()),
            cpu_governor: Some(self.env.governor.clone()),
            avail_memory_mb: i64::try_from(self.env.memory_available_mb).ok(),
            storage_notes: self.storage_notes.clone(),
            cli_args: config.cli_args.clone(),
            project: self.project.name().to_owned(),
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

    if let Some(ref v) = config.variant {
        parts.push(format!("variant={v}"));
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

/// Return the variant suffix for hotpath mode: `"/alloc"` or `""`.
pub fn hotpath_variant_suffix(alloc: bool) -> &'static str {
    if alloc { "/alloc" } else { "" }
}

/// Convert a `Duration` to milliseconds as `i64`.
pub fn elapsed_to_ms(duration: &Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// Run a binary with hotpath env vars, capture the JSON report, and return a `BenchResult`.
///
/// This is the shared inner loop for all hotpath profiling commands. Sets
/// `HOTPATH_METRICS_SERVER_OFF`, `HOTPATH_OUTPUT_FORMAT`, and `HOTPATH_OUTPUT_PATH`,
/// then reads and parses the resulting JSON report file.
pub fn run_hotpath_capture(
    binary: &str,
    args: &[&str],
    scratch_dir: &std::path::Path,
    project_root: &std::path::Path,
    extra_env: &[(&str, &str)],
) -> Result<(BenchResult, Vec<u8>), crate::error::DevError> {
    let json_file = scratch_dir.join("hotpath-report.json");
    let json_file_str = json_file.display().to_string();

    let mut env: Vec<(&str, &str)> = vec![
        ("HOTPATH_METRICS_SERVER_OFF", "true"),
        ("HOTPATH_OUTPUT_FORMAT", "json"),
        ("HOTPATH_OUTPUT_PATH", &json_file_str),
    ];
    env.extend_from_slice(extra_env);

    let captured = output::run_captured_with_env(binary, args, project_root, &env)?;

    captured.check_success(binary)?;

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

/// Pick the smaller of two millisecond values.
fn pick_best_ms(current: Option<i64>, candidate: i64) -> i64 {
    match current {
        Some(best) if best <= candidate => best,
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

#[cfg(test)]
mod tests {
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
        // "tag=" has empty value — not parseable as i64 or f64, so becomes a string
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

    #[test]
    fn pick_best_ms_none_vs_candidate() {
        assert_eq!(pick_best_ms(None, 42), 42);
    }

    #[test]
    fn pick_best_ms_keeps_smaller() {
        assert_eq!(pick_best_ms(Some(10), 20), 10);
    }

    #[test]
    fn pick_best_ms_replaces_with_smaller() {
        assert_eq!(pick_best_ms(Some(20), 10), 10);
    }

    #[test]
    fn pick_best_ms_equal_keeps_current() {
        assert_eq!(pick_best_ms(Some(5), 5), 5);
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
    // hotpath_feature / hotpath_variant_suffix
    // -----------------------------------------------------------------------

    #[test]
    fn hotpath_feature_without_alloc() {
        assert_eq!(hotpath_feature(false), "hotpath");
    }

    #[test]
    fn hotpath_feature_with_alloc() {
        assert_eq!(hotpath_feature(true), "hotpath-alloc");
    }

    #[test]
    fn hotpath_variant_suffix_without_alloc() {
        assert_eq!(hotpath_variant_suffix(false), "");
    }

    #[test]
    fn hotpath_variant_suffix_with_alloc() {
        assert_eq!(hotpath_variant_suffix(true), "/alloc");
    }

    #[test]
    fn hotpath_feature_and_suffix_are_consistent() {
        // The feature name should contain "alloc" iff the suffix does
        for alloc in [true, false] {
            let feature = hotpath_feature(alloc);
            let suffix = hotpath_variant_suffix(alloc);
            assert_eq!(
                feature.contains("alloc"),
                suffix.contains("alloc"),
                "feature={feature} and suffix={suffix} should agree on alloc presence"
            );
        }
    }
}
