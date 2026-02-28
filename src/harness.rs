use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::DriveConfig;
use crate::db::{ResultsDb, RunRow};
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
    pub metadata: Option<serde_json::Value>,
}

/// Result of a single benchmark measurement.
pub struct BenchResult {
    pub elapsed_ms: i64,
    pub extra: Option<serde_json::Value>,
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
}

impl BenchHarness {
    /// Create a new harness, acquiring the lockfile and collecting environment.
    pub fn new(
        paths: &crate::config::ResolvedPaths,
        project_root: &Path,
        project: crate::project::Project,
        lock_command: &str,
    ) -> Result<Self, DevError> {
        std::fs::create_dir_all(&paths.scratch_dir)?;
        let lock = crate::lockfile::acquire(&crate::lockfile::LockContext {
            project: project.name(),
            command: lock_command,
            project_root: &project_root.display().to_string(),
        })?;
        let env = crate::env::collect(paths, project, project_root);
        let git = crate::git::collect(project_root)?;
        let db_dir = project_root.join(".brokkr");
        std::fs::create_dir_all(&db_dir)?;
        let db = ResultsDb::open(&db_dir.join("results.db"))?;
        let storage_notes = format_storage_notes(&paths.drives);

        if !git.is_clean {
            output::bench_msg(
                "dirty tree — results go to stdout only, not stored in database",
            );
        }

        Ok(Self {
            _lock: lock,
            db,
            env,
            git,
            storage_notes,
        })
    }

    /// Internal timing: closure called N times, returns `BenchResult`.
    /// Best-of-N (minimum `elapsed_ms`).
    pub fn run_internal<F>(
        &self,
        config: &BenchConfig,
        f: F,
    ) -> Result<BenchResult, DevError>
    where
        F: Fn(usize) -> Result<BenchResult, DevError>,
    {
        let mut best: Option<BenchResult> = None;

        for i in 0..config.runs {
            output::bench_msg(&format!("run {}/{}", i + 1, config.runs));
            let result = f(i)?;
            best = Some(pick_best(best, result));
        }

        let best = best.ok_or_else(|| {
            DevError::Config("benchmark requires at least 1 run".into())
        })?;

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
            let captured = output::run_captured(
                &program.display().to_string(),
                args,
                cwd,
            )?;
            let ms = elapsed_to_ms(&start.elapsed());

            captured.check_success(&program.display().to_string())?;

            best_ms = Some(pick_best_ms(best_ms, ms));
        }

        let elapsed_ms = best_ms.ok_or_else(|| {
            DevError::Config("benchmark requires at least 1 run".into())
        })?;

        let result = BenchResult {
            elapsed_ms,
            extra: None,
        };
        self.record_result(config, &result)?;
        Ok(result)
    }

    /// Distribution timing: collect all N samples, compute min/p50/p95/max.
    pub fn run_distribution<F>(
        &self,
        config: &BenchConfig,
        f: F,
    ) -> Result<BenchResult, DevError>
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

        let extra = serde_json::json!({
            "min_ms": min,
            "p50_ms": p50,
            "p95_ms": p95,
            "max_ms": max,
            "samples": samples.len(),
        });

        let result = BenchResult {
            elapsed_ms: min,
            extra: Some(extra),
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
        let mut best: Option<BenchResult> = None;

        for i in 0..config.runs {
            output::bench_msg(&format!("run {}/{}", i + 1, config.runs));

            let captured = output::run_captured(
                &program.display().to_string(),
                args,
                cwd,
            )?;

            captured.check_success(&program.display().to_string())?;

            let result = parse_kv_stderr(&captured.stderr)?;
            best = Some(pick_best(best, result));
        }

        let best = best.ok_or_else(|| {
            DevError::Config("benchmark requires at least 1 run".into())
        })?;

        self.record_result(config, &best)?;
        Ok(best)
    }

    // -----------------------------------------------------------------------
    // Private methods
    // -----------------------------------------------------------------------

    /// Record a result: always emit to stdout, store in DB if tree is clean.
    /// Prints the short UUID to stdout (always, regardless of quiet mode).
    fn record_result(
        &self,
        config: &BenchConfig,
        result: &BenchResult,
    ) -> Result<(), DevError> {
        if self.git.is_clean {
            let row = self.build_row(config, result);
            let short = self.db.insert(&row)?;
            emit_result_lines(config, result, &self.git);
            output::bench_msg(&format!("stored in results.db ({short})"));
            println!("{short}");
        } else {
            // Dirty tree: no DB insert, no UUID. Always print result line
            // since the data can't be looked up later.
            force_emit_result_lines(config, result, &self.git);
        }

        Ok(())
    }

    /// Build a `RunRow` from harness state, config, and result.
    fn build_row(&self, config: &BenchConfig, result: &BenchResult) -> RunRow {
        RunRow {
            hostname: self.env.hostname.clone(),
            commit: self.git.commit.clone(),
            subject: self.git.subject.clone(),
            command: config.command.clone(),
            variant: config.variant.clone(),
            input_file: config.input_file.clone(),
            input_mb: config.input_mb,
            cargo_features: config.cargo_features.clone(),
            cargo_profile: config.cargo_profile.clone(),
            elapsed_ms: result.elapsed_ms,
            kernel: Some(self.env.kernel.clone()),
            cpu_governor: Some(self.env.governor.clone()),
            avail_memory_mb: i64::try_from(self.env.memory_available_mb).ok(),
            storage_notes: self.storage_notes.clone(),
            extra: result.extra.clone(),
            cli_args: config.cli_args.clone(),
            metadata: config.metadata.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Build a result summary string with key=value pairs.
fn format_result_line(
    config: &BenchConfig,
    result: &BenchResult,
    git: &GitInfo,
) -> String {
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

    append_extra_fields(&mut parts, &result.extra);

    parts.join("  ")
}

/// Emit a `[result]` line (respects quiet mode).
fn emit_result_lines(
    config: &BenchConfig,
    result: &BenchResult,
    git: &GitInfo,
) {
    output::result_msg(&format_result_line(config, result, git));
}

/// Emit a `[result]` line unconditionally (ignores quiet mode).
/// Used for dirty-tree results that can't be looked up later.
fn force_emit_result_lines(
    config: &BenchConfig,
    result: &BenchResult,
    git: &GitInfo,
) {
    println!("[result]  {}", format_result_line(config, result, git));
}

/// Flatten top-level keys from the extra JSON object into the result line.
fn append_extra_fields(parts: &mut Vec<String>, extra: &Option<serde_json::Value>) {
    let Some(serde_json::Value::Object(map)) = extra else {
        return;
    };

    for (key, value) in map {
        let formatted = format_json_value(value);
        parts.push(format!("{key}={formatted}"));
    }
}

/// Format a JSON value for display in a result line.
fn format_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_owned(),
        other => other.to_string(),
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
) -> Result<BenchResult, crate::error::DevError> {
    let json_file = scratch_dir.join("hotpath-report.json");
    let json_file_str = json_file.display().to_string();

    let captured = output::run_captured_with_env(
        binary,
        args,
        project_root,
        &[
            ("HOTPATH_METRICS_SERVER_OFF", "true"),
            ("HOTPATH_OUTPUT_FORMAT", "json"),
            ("HOTPATH_OUTPUT_PATH", &json_file_str),
        ],
    )?;

    captured.check_success(binary)?;

    let ms = elapsed_to_ms(&captured.elapsed);

    let extra = std::fs::read_to_string(&json_file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    std::fs::remove_file(&json_file).ok();

    Ok(BenchResult {
        elapsed_ms: ms,
        extra,
    })
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
    { result.round() as i64 }
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
/// puts all other kv pairs into `BenchResult.extra` as a JSON object.
fn parse_kv_stderr(stderr: &[u8]) -> Result<BenchResult, DevError> {
    let text = String::from_utf8_lossy(stderr);
    let mut elapsed_ms: Option<i64> = None;
    let mut extra = serde_json::Map::new();

    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if key == "elapsed_ms" || key == "total_ms" {
                elapsed_ms = Some(value.parse().map_err(|_| {
                    DevError::Config(format!("invalid elapsed_ms value: {value}"))
                })?);
            } else if let Ok(n) = value.parse::<i64>() {
                extra.insert(key.to_owned(), serde_json::Value::Number(n.into()));
            } else if let Ok(f) = value.parse::<f64>() {
                if let Some(n) = serde_json::Number::from_f64(f) {
                    extra.insert(key.to_owned(), serde_json::Value::Number(n));
                } else {
                    extra.insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
                }
            } else {
                extra.insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
            }
        }
    }

    let elapsed_ms = elapsed_ms.ok_or_else(|| {
        let preview: String = text.chars().take(500).collect();
        DevError::Config(format!(
            "subprocess stderr missing elapsed_ms=NNN. stderr was:\n{preview}"
        ))
    })?;

    Ok(BenchResult {
        elapsed_ms,
        extra: if extra.is_empty() { None } else { Some(serde_json::Value::Object(extra)) },
    })
}
