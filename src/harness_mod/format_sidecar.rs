
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
    let child = output::spawn_captured(binary, args, project_root, &env, true)?;
    if let Some(lock) = lock {
        lock.set_child_pid(child.id());
    }
    let sidecar_result = crate::sidecar::run_sidecar(child, &mut fifo, 0, start, stop_marker);
    if let Some(lock) = lock {
        lock.clear_child_pid();
    }
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

