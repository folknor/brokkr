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
#[derive(Debug)]
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
    cargo_features: Option<String>,
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
    ) -> Result<Self, DevError> {
        let lock = crate::lockfile::acquire(&crate::lockfile::LockContext {
            project: project.name(),
            command: lock_command,
            project_root: &project_root.display().to_string(),
        })?;
        Self::new_with_lock(lock, paths, project_root, db_root, project)
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
            output::error(
                "WARNING: dirty tree — results will NOT be stored in database",
            );
        }

        Ok(Self {
            _lock: lock,
            db,
            env,
            git,
            storage_notes,
            cargo_features: None,
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
    pub fn record_result(
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
            output::error("NOT STORED — tree is dirty (commit or stash changes)");
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
            cargo_features: config.cargo_features.clone().or_else(|| self.cargo_features.clone()),
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
) -> Result<BenchResult, crate::error::DevError> {
    let json_file = scratch_dir.join("hotpath-report.json");
    let json_file_str = json_file.display().to_string();

    let mut env: Vec<(&str, &str)> = vec![
        ("HOTPATH_METRICS_SERVER_OFF", "true"),
        ("HOTPATH_OUTPUT_FORMAT", "json"),
        ("HOTPATH_OUTPUT_PATH", &json_file_str),
    ];
    env.extend_from_slice(extra_env);

    let captured = output::run_captured_with_env(
        binary,
        args,
        project_root,
        &env,
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
        assert_eq!(percentile(&data, 50), 150, "midpoint should interpolate to 150");
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
        assert_eq!(percentile(&data, 25), 20, "p25 of [10,20,30,40,50] should be 20");
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
        assert_eq!(p95, 910, "linear interpolation should yield 910, not nearest-rank 1000");
        assert!(p95 < 1000, "interpolated p95 must be less than max for non-degenerate data");
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
        assert!(result.extra.is_none(), "no extra fields => extra should be None");
    }

    #[test]
    fn parse_kv_stderr_total_ms_alias() {
        let stderr = b"total_ms=999\n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(result.elapsed_ms, 999, "total_ms should be accepted as elapsed_ms alias");
    }

    #[test]
    fn parse_kv_stderr_elapsed_ms_takes_precedence_over_total_ms() {
        // Both present: last one wins (due to overwrite semantics)
        let stderr = b"total_ms=100\nelapsed_ms=200\n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(result.elapsed_ms, 200, "elapsed_ms should overwrite earlier total_ms");

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

        let extra = result.extra.unwrap();
        let map = extra.as_object().unwrap();

        // Integer field
        assert_eq!(map["rows"], serde_json::json!(42));
        // Float field
        assert_eq!(map["rate"], serde_json::json!(3.14));
        // String field
        assert_eq!(map["label"], serde_json::json!("fast"));
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
        assert_eq!(result.elapsed_ms, 777, "should find elapsed_ms among garbage lines");
    }

    #[test]
    fn parse_kv_stderr_empty_value_treated_as_string() {
        // "tag=" has empty value — not parseable as i64 or f64, so becomes a string
        let stderr = b"elapsed_ms=100\ntag=\n";
        let result = parse_kv_stderr(stderr).unwrap();
        let extra = result.extra.unwrap();
        let map = extra.as_object().unwrap();
        assert_eq!(map["tag"], serde_json::json!(""), "empty value should become empty string");
    }

    #[test]
    fn parse_kv_stderr_whitespace_trimming() {
        let stderr = b"  elapsed_ms  =  300  \n  count  =  5  \n";
        let result = parse_kv_stderr(stderr).unwrap();
        assert_eq!(result.elapsed_ms, 300, "keys and values should be trimmed");
        let extra = result.extra.unwrap();
        assert_eq!(extra["count"], serde_json::json!(5));
    }

    #[test]
    fn parse_kv_stderr_invalid_elapsed_ms_value() {
        let stderr = b"elapsed_ms=not_a_number\n";
        match parse_kv_stderr(stderr) {
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("invalid elapsed_ms value"),
                    "should report invalid value, got: {msg}"
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
        let extra = result.extra.unwrap();
        assert_eq!(extra["weird"], serde_json::json!("NaN"), "NaN should fall through to string");
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
        assert_eq!(maybe_quote(""), "", "empty string has no spaces, should not be quoted");
    }

    // -----------------------------------------------------------------------
    // pick_best / pick_best_ms
    // -----------------------------------------------------------------------

    #[test]
    fn pick_best_none_vs_candidate() {
        let candidate = BenchResult { elapsed_ms: 500, extra: None };
        let result = pick_best(None, candidate);
        assert_eq!(result.elapsed_ms, 500, "None current should always take candidate");
    }

    #[test]
    fn pick_best_keeps_better() {
        let current = BenchResult { elapsed_ms: 100, extra: None };
        let candidate = BenchResult { elapsed_ms: 200, extra: None };
        let result = pick_best(Some(current), candidate);
        assert_eq!(result.elapsed_ms, 100, "should keep the lower value");
    }

    #[test]
    fn pick_best_replaces_with_better() {
        let current = BenchResult { elapsed_ms: 300, extra: None };
        let candidate = BenchResult { elapsed_ms: 150, extra: None };
        let result = pick_best(Some(current), candidate);
        assert_eq!(result.elapsed_ms, 150, "should replace with lower value");
    }

    #[test]
    fn pick_best_equal_keeps_current() {
        // Tie-breaking: current wins (<=)
        let current = BenchResult {
            elapsed_ms: 100,
            extra: Some(serde_json::json!({"tag": "first"})),
        };
        let candidate = BenchResult {
            elapsed_ms: 100,
            extra: Some(serde_json::json!({"tag": "second"})),
        };
        let result = pick_best(Some(current), candidate);
        assert_eq!(
            result.extra.unwrap()["tag"], "first",
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
