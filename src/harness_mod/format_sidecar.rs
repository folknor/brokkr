
// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Build a result summary string with key=value pairs.
///
/// `mode` is the harness-resolved measurement mode
/// ([`BenchHarness::effective_mode`]), not `config.mode` - the per-config
/// field is an override no writer sets, so reading it alone meant `mode=`
/// never printed.
fn format_result_line(
    config: &BenchConfig,
    mode: Option<&str>,
    result: &BenchResult,
    git: &GitInfo,
) -> String {
    let mut parts = Vec::with_capacity(8);
    parts.push(format!("command={}", config.command));

    if let Some(v) = mode {
        parts.push(format!("mode={v}"));
    }

    // Print the exact figure when the target reported one - rounding a 6.847
    // ms region to `elapsed_ms=7` on the console would hide precisely the
    // signal a sub-millisecond workload is being measured for.
    parts.push(format!(
        "elapsed_ms={}",
        ms_value(result.elapsed_ms, result.elapsed_us)
    ));
    parts.push(format!("commit={}", git.commit));

    if let Some(ref input) = config.input_file {
        parts.push(format!("input={input}"));
    }

    append_kv_fields(&mut parts, &result.kv);

    // Compute I/O throughput when input size and elapsed time are known -
    // from the microsecond wall when there is one, so a run that rounds to
    // 0 ms still gets a figure instead of silently skipping the line.
    let wall_us = bench_ordering_key(result);
    if let Some(input_mb) = config.input_mb
        && wall_us > 0
    {
        #[allow(clippy::cast_precision_loss)]
        let secs = wall_us as f64 / 1_000_000.0;
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
        let us = dist.us;
        parts.push(format!("samples={}", dist.samples));
        parts.push(format!("min_ms={}", ms_value(dist.min_ms, us.map(|u| u.min))));
        parts.push(format!("p50_ms={}", ms_value(dist.p50_ms, us.map(|u| u.p50))));
        parts.push(format!("p95_ms={}", ms_value(dist.p95_ms, us.map(|u| u.p95))));
        parts.push(format!("max_ms={}", ms_value(dist.max_ms, us.map(|u| u.max))));
    }

    parts.join("  ")
}

/// A millisecond value for a `[result]` field: three decimals from the
/// microsecond reading when there is one (`0.312`), else the integer.
fn ms_value(ms: i64, us: Option<i64>) -> String {
    match us {
        Some(us) => {
            #[allow(clippy::cast_precision_loss)]
            let exact = us as f64 / 1000.0;
            format!("{exact:.3}")
        }
        None => ms.to_string(),
    }
}

/// Emit a `[result]` line (respects quiet mode).
fn emit_result_lines(
    config: &BenchConfig,
    mode: Option<&str>,
    result: &BenchResult,
    git: &GitInfo,
) {
    output::result_msg(&format_result_line(config, mode, result, git));
}

/// Emit a `[result]` line unconditionally (ignores quiet mode).
/// Used for results that can't be looked up later: a dirty tree, or a
/// clean one whose results.db insert failed.
fn force_emit_result_lines(
    config: &BenchConfig,
    mode: Option<&str>,
    result: &BenchResult,
    git: &GitInfo,
) {
    println!("[result]  {}", format_result_line(config, mode, result, git));
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
/// Each failure is printed live with its cause. On completion, returns
/// `Ok(())` if all succeeded, or a bare summary error naming which variants
/// failed.
///
/// An empty `variants` list is an error, not a vacuous success. Callers build
/// the list by filtering a fixed set against a user selector (`--query NAME`,
/// `--mode NAME`, ...), so an empty list means the selector matched nothing -
/// a typo. Returning `Ok` there recorded no rows and exited 0, which reads
/// exactly like a successful bench.
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
    if variants.is_empty() {
        return Err(DevError::Refused(format!(
            "no {label} variants selected - nothing to benchmark (check the {label} name)"
        )));
    }

    let mut failures: Vec<&str> = Vec::new();

    for &variant in variants {
        output::bench_msg(&format!("{label}: {variant}"));
        match run_one(variant) {
            Ok(()) => {}
            // A cooperative shutdown ends the whole run, not one variant.
            Err(e @ DevError::Interrupted) => return Err(e),
            Err(e) => {
                output::error(&format!("{variant} failed: {e}"));
                failures.push(variant);
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(variants_failed(&failures, variants.len()))
    }
}

/// The summary error [`run_variants`] ends with. Each failure's cause was
/// already printed live as `<variant> failed: ...`, so this names the variants
/// only and renders bare (`Reported`) rather than repeating the causes under a
/// `verify:` prefix.
fn variants_failed(failed: &[&str], total: usize) -> DevError {
    DevError::Reported(format!(
        "{} of {} variants failed: {}",
        failed.len(),
        total,
        failed.join(", "),
    ))
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

/// Convert a `Duration` to milliseconds as `i64`, rounded to nearest.
///
/// Nearest, not floored, for the reason [`us_to_ms`] gives: flooring reads
/// every run as up to a whole millisecond faster than it was. Saturates at
/// `i64::MAX`.
pub fn elapsed_to_ms(duration: &Duration) -> i64 {
    i64::try_from(duration.as_micros().saturating_add(500) / 1000).unwrap_or(i64::MAX)
}

/// Convert a `Duration` to whole microseconds as `i64`. Saturates at
/// `i64::MAX`.
///
/// The externally timed paths measure an exact `Duration`; this is what
/// they record as `elapsed_us`, so best-of-N compares microseconds rather
/// than millisecond ties.
pub fn elapsed_to_us(duration: &Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}

/// A `run_hotpath_capture` that failed after its child ran, parked for the
/// enclosing [`BenchHarness::run_hotpath`] loop.
///
/// The capture has no harness (no `sidecar.db` path), and its callers
/// propagate its error with `?` out of the loop's closure, so the
/// `/proc` trajectory it collected - the most useful data a crashed or
/// OOM-killed run leaves - used to be dropped with the error. Parking it
/// here lets the loop store it under `dirty`, as `run_external_*` does,
/// without changing the closure contract every hotpath writer uses.
struct FailedCapture {
    data: crate::sidecar::SidecarData,
    pid: u32,
    exit_code: i32,
}

thread_local! {
    /// At most one parked failure: the loop clears the slot before each
    /// iteration and takes it on the iteration's error. Thread-local
    /// because the closure runs on the loop's own thread.
    static FAILED_CAPTURE: std::cell::RefCell<Option<FailedCapture>> =
        const { std::cell::RefCell::new(None) };
}

fn park_failed_capture(capture: FailedCapture) {
    FAILED_CAPTURE.with(|slot| *slot.borrow_mut() = Some(capture));
}

fn take_failed_capture() -> Option<FailedCapture> {
    FAILED_CAPTURE.with(|slot| slot.borrow_mut().take())
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
    let pid = child.id();
    if let Some(lock) = lock {
        lock.set_child_pid(pid);
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
    // Failures park the sidecar data for the enclosing `run_hotpath` loop,
    // which stores it under `dirty` (see `FailedCapture`).
    if interrupted {
        park_failed_capture(FailedCapture {
            data: sidecar_result.data,
            pid,
            exit_code: exit_code_from_status(&captured.status),
        });
        return Err(crate::error::DevError::Interrupted);
    }
    if !stopped && let Err(e) = captured.check_success_or(binary, ok_codes) {
        park_failed_capture(FailedCapture {
            data: sidecar_result.data,
            pid,
            exit_code: exit_code_from_status(&captured.status),
        });
        return Err(e);
    }

    let ms = elapsed_to_ms(&captured.elapsed);
    let us = elapsed_to_us(&captured.elapsed);
    let (_stderr_ms, kv) = parse_kv_lines(&captured.stderr);
    let stderr = captured.stderr;

    let hotpath = match std::fs::read_to_string(&json_file) {
        Ok(s) => match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(v) => db::hotpath_data_from_json(&v),
            Err(e) => {
                output::warn(&format!("failed to parse hotpath JSON: {e}"));
                None
            }
        },
        Err(e) => {
            output::warn(&format!(
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
            elapsed_us: Some(us),
            kv,
            // Single capture - the enclosing run_hotpath loop owns the list.
            iterations: Vec::new(),
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

/// Summarise distribution samples, given in microseconds in any order, as
/// min/p50/p95/max. Returns the summary and the fastest sample, or `None`
/// when there are no samples.
///
/// The percentiles are taken over the microseconds and each `*_ms` field is
/// its `*_us` counterpart rounded to nearest (`us_to_ms`), so the two
/// columns of one row can never disagree by more than the rounding.
fn summarize_distribution(samples_us: &[i64]) -> Option<(Distribution, i64)> {
    if samples_us.is_empty() {
        return None;
    }
    let mut sorted = samples_us.to_vec();
    sorted.sort_unstable();
    let us = DistributionUs {
        min: percentile(&sorted, 0),
        p50: percentile(&sorted, 50),
        p95: percentile(&sorted, 95),
        max: percentile(&sorted, 100),
    };
    let dist = Distribution {
        samples: i64::try_from(sorted.len()).unwrap_or(i64::MAX),
        min_ms: us_to_ms(us.min),
        p50_ms: us_to_ms(us.p50),
        p95_ms: us_to_ms(us.p95),
        max_ms: us_to_ms(us.max),
        us: Some(us),
    };
    Some((dist, us.min))
}

/// Pick the faster `BenchResult`.
///
/// Compares microseconds when both sides have them - on a single-digit
/// millisecond workload, best-of-N on rounded milliseconds is mostly a
/// coin toss between ties.
fn pick_best(current: Option<BenchResult>, candidate: BenchResult) -> BenchResult {
    match current {
        Some(best) if bench_ordering_key(&best) <= bench_ordering_key(&candidate) => best,
        _ => candidate,
    }
}

/// Comparable wall for best-of-N, in microseconds.
fn bench_ordering_key(result: &BenchResult) -> i64 {
    result
        .elapsed_us
        .unwrap_or_else(|| result.elapsed_ms.saturating_mul(1000))
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
    let (us, kv) = parse_kv_lines_us(stderr);
    (us.map(us_to_ms), kv)
}

/// Round microseconds to the nearest millisecond.
///
/// Nearest, not truncating: a 6.847 ms region is 7 ms, not 6. `elapsed_ms`
/// is a display and back-compat value once `elapsed_us` exists, so being off
/// by a whole millisecond in the direction of "faster than it was" is the
/// one error worth avoiding.
fn us_to_ms(us: i64) -> i64 {
    // Saturating: `elapsed_to_us` saturates at `i64::MAX`, and the plain
    // addition would then overflow.
    us.saturating_add(500) / 1000
}

/// Per-iteration microsecond walls, kept only when every iteration had one.
///
/// A list with gaps cannot stay index-aligned with the millisecond walls, and
/// the alignment - execution order - is the only reason the list exists, so a
/// single missing reading drops the whole list rather than part of it.
fn all_or_none(walls_us: Vec<Option<i64>>) -> Vec<i64> {
    walls_us
        .into_iter()
        .collect::<Option<Vec<i64>>>()
        .unwrap_or_default()
}

/// As [`parse_kv_lines`], but returns the timing in microseconds.
///
/// The timing line may be fractional (`elapsed_ms=6.847`). It used to be
/// parsed as `i64`, so a fractional value failed to parse at all and the run
/// then died on the "missing elapsed_ms" check - a target reporting *more*
/// precision than brokkr asked for was treated as reporting none.
pub(crate) fn parse_kv_lines_us(stderr: &[u8]) -> (Option<i64>, Vec<KvPair>) {
    let text = String::from_utf8_lossy(stderr);
    let mut elapsed_us: Option<i64> = None;
    let mut kv = Vec::new();

    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if key == "elapsed_ms" || key == "total_ms" {
                // Integer first so whole-millisecond values stay exact rather
                // than making a round trip through f64.
                if let Ok(ms) = value.parse::<i64>() {
                    elapsed_us = Some(ms.saturating_mul(1000));
                } else if let Ok(ms) = value.parse::<f64>()
                    && ms.is_finite()
                    && ms >= 0.0
                {
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        elapsed_us = Some((ms * 1000.0).round() as i64);
                    }
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

    (elapsed_us, kv)
}

fn parse_kv_stderr(stderr: &[u8]) -> Result<BenchResult, DevError> {
    let (elapsed_us, kv) = parse_kv_lines_us(stderr);

    let elapsed_us = elapsed_us.ok_or_else(|| {
        let text = String::from_utf8_lossy(stderr);
        let preview: String = text.chars().take(500).collect();
        DevError::Config(format!(
            "subprocess stderr missing elapsed_ms=NNN. stderr was:\n{preview}"
        ))
    })?;

    Ok(BenchResult {
        elapsed_ms: us_to_ms(elapsed_us),
        // This is the one path that knows sub-millisecond timing: the target
        // reported it directly.
        elapsed_us: Some(elapsed_us),
        kv,
        // One iteration's parse - the enclosing loop owns the list.
        iterations: Vec::new(),
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
/// `$HOME/.local/share/brokkr/sidecar-backups/`. An empty or relative
/// `XDG_DATA_HOME` counts as unset (`user_dirs`), so the backups can never
/// land in - and dirty - the tree brokkr is running in.
fn sidecar_backup_dir() -> Result<PathBuf, DevError> {
    let data_dir = crate::user_dirs::xdg_data_home().ok_or_else(|| {
        DevError::Config("cannot determine data directory for sidecar backup".into())
    })?;
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
    // The new backup is staged beside `base` and promoted by
    // `atomic_write::Staged::commit` (fsync, rename, dir fsync). Dropping
    // `staged` on any early error removes the temp file.
    let staged = crate::atomic_write::Staged::stable(&base)?;

    // Clean up any stale temp from a previous interrupted run: the backup
    // API writes into an existing file rather than replacing it.
    if staged.tmp_path().exists() {
        std::fs::remove_file(staged.tmp_path()).ok();
    }

    // Create backup via SQLite backup API. This reads the logical DB state
    // (including uncommitted WAL pages from other connections) and writes a
    // self-contained DELETE-journal-mode database at the temp path. The
    // backup API also runs quick_check on the result.
    crate::db::sidecar::backup_to_path(sidecar_path, staged.tmp_path())?;

    // Promote the new backup into the primary slot without displacing the
    // current primary until the new one is in place.
    //
    // Sequence:
    //   1. Shift older copies: .1 → .2 (clears .1 slot, drops oldest)
    //   2. Preserve current primary: hard-link base → .1
    //   3. Atomic promote: commit the staged file → base (overwrites old base)
    //
    // Every step propagates errors. If any rotation or preservation step
    // fails, the backup is considered failed rather than silently losing
    // retention history.

    // Shift older copies: .1 → .2, .2 → .3, etc.
    // This clears the .1 slot so the hard-link in the next step can
    // succeed without a prior remove. These renames *move* finished copies
    // between retention slots - not a temp-and-replace - so they stay raw;
    // the final commit's directory fsync makes them durable.
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

    // Atomic promotion: the staged file replaces base. If this fails, base
    // is still the old copy (the hard-link in step 2 created .1 as a second
    // link to the same inode, so the data is preserved regardless).
    staged.commit()?;

    output::sidecar_msg(&format!("backup: {}", base.display()));
    Ok(())
}

