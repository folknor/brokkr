use crate::sidecar::linear_percentile as percentile;

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Build a result summary string with key=value pairs.
///
/// `mode` is the session's measurement mode, shared by rows and sidecar records.
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
        ms_value(result_wall(result.elapsed_ms, result.elapsed_us))
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
        parts.push(format!("samples={}", dist.samples));
        parts.push(format!("min_ms={}", ms_value(dist.min)));
        parts.push(format!("p50_ms={}", ms_value(dist.p50)));
        parts.push(format!("p95_ms={}", ms_value(dist.p95)));
        parts.push(format!("max_ms={}", ms_value(dist.max)));
    }

    parts.join("  ")
}

/// A millisecond value for a `[result]` field: three decimals from the
/// microsecond reading when there is one (`0.312`), else the integer.
fn ms_value(wall: Wall) -> String {
    match wall.micros() {
        Some(us) => {
            #[allow(clippy::cast_precision_loss)]
            let exact = us as f64 / 1000.0;
            format!("{exact:.3}")
        }
        None => wall.ms().to_string(),
    }
}

/// The stored wall for a harness result: exact when the path measured
/// microseconds, otherwise the millisecond-only reading.
fn result_wall(elapsed_ms: i64, elapsed_us: Option<i64>) -> Wall {
    Wall::from_columns(elapsed_ms, elapsed_us)
}

/// Emit a `[result]` line (respects quiet mode).
fn emit_result_lines(
    config: &BenchConfig,
    mode: Option<&str>,
    result: &BenchResult,
    git: &GitInfo,
) {
    console::result_msg(&format_result_line(config, mode, result, git));
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
    console::plain(&format!("[result]  {}", format_result_line(config, mode, result, git)));
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

fn clamp_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The line a harness loop prints before iteration `i` (zero-based) of `runs`.
/// Silent for a single run: "run 1/1" says nothing.
fn run_progress_line(i: usize, runs: usize) -> Option<String> {
    (runs > 1).then(|| format!("run {}/{}", i + 1, runs))
}

/// The single `[sidecar]` line a stored profile prints: the best run's
/// sample, marker and counter counts (with the run number when there were
/// several). `dirty` marks a trajectory filed under the `dirty` alias because
/// no results row exists for it; only then is the id named here, since
/// otherwise it is the results row's id, already printed.
fn sidecar_summary_line(
    short: &str,
    dirty: bool,
    runs: &[&crate::sidecar::SidecarData],
    best_run_idx: usize,
) -> String {
    let which = if runs.len() > 1 {
        format!("best run {}/{}: ", best_run_idx + 1, runs.len())
    } else {
        String::new()
    };
    let counts = runs.get(best_run_idx).map_or_else(String::new, |d| {
        format!(
            "{which}{} samples, {} markers, {} counters ",
            d.samples.len(),
            d.markers.len(),
            d.counters.len()
        )
    });
    // A profile with a results row shares its id, which the `results.db` line
    // already printed; only a `dirty`-filed one needs its own.
    if dirty {
        format!("{counts}(sidecar.db {short}, filed as dirty)")
    } else {
        format!("{counts}(sidecar.db)")
    }
}

/// Print [`run_progress_line`] as bench narration, if there is one.
pub(crate) fn announce_run(i: usize, runs: usize) {
    if let Some(line) = run_progress_line(i, runs) {
        console::bench_msg(&line);
    }
}

/// Extract an exit code from a process `ExitStatus`.
///
/// Returns the exit code if the process exited normally, or `128 + signal`
/// if it was killed by a signal (matching shell convention: 137 = OOM kill).
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
        console::bench_msg(&format!("{label}: {variant}"));
        match run_one(variant) {
            Ok(()) => {}
            // A cooperative shutdown ends the whole run, not one variant.
            Err(e @ DevError::Interrupted) => return Err(e),
            Err(e) => {
                console::error(&format!("{variant} failed: {e}"));
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

/// Summarise distribution samples, given in microseconds in any order, as
/// min/p50/p95/max. Returns the summary and the fastest sample, or `None`
/// when there are no samples.
///
/// The percentiles are taken over the microseconds and kept exact; the
/// millisecond column is derived from them by [`Wall::ms`]'s nearest
/// rounding, so the two columns of one row can never disagree.
fn summarize_distribution(samples_us: &[i64]) -> Option<(Distribution, i64)> {
    if samples_us.is_empty() {
        return None;
    }
    let mut sorted = samples_us.to_vec();
    sorted.sort_unstable();
    let min = percentile(&sorted, 0);
    let dist = Distribution {
        samples: i64::try_from(sorted.len()).unwrap_or(i64::MAX),
        min: Wall::from_micros(min),
        p50: Wall::from_micros(percentile(&sorted, 50)),
        p95: Wall::from_micros(percentile(&sorted, 95)),
        max: Wall::from_micros(percentile(&sorted, 100)),
    };
    Some((dist, min))
}

/// Keep the first minimum, comparing exact walls where available.
fn winning_index(current: Option<(usize, &BenchResult)>, index: usize, candidate: &BenchResult) -> usize {
    match current {
        Some((winner, best)) if bench_ordering_key(best) <= bench_ordering_key(candidate) => winner,
        _ => index,
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

/// Parse stderr `key=value` lines, returning the timing and non-timing pairs.
/// `elapsed_ms`/`total_ms` are reserved timing names and are not added to the
/// returned kv pairs; the last valid timing line wins.
///
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
        let preview: String = text
            .chars()
            .take(crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES.stderr_preview_chars)
            .collect();
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
const SIDECAR_BACKUP_COPIES: usize =
    crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES.sidecar_backup_copies;

/// Resolve the sidecar backup directory.
///
/// Uses `$XDG_DATA_HOME/brokkr/sidecar-backups/`, falling back to
/// `$HOME/.local/share/brokkr/sidecar-backups/`. An empty or relative
/// `XDG_DATA_HOME` counts as unset (`user_dirs`), so the backups can never
/// land in - and dirty - the tree brokkr is running in.
fn sidecar_backup_dir() -> Result<PathBuf, DevError> {
    crate::user_dirs::sidecar_backups_dir().ok_or_else(|| {
        DevError::Config("cannot determine data directory for sidecar backup".into())
    })
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

    console::sidecar_msg(&format!("backup: {}", base.display()));
    Ok(())
}

