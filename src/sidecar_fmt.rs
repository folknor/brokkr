//! Formatting and filtering helpers for sidecar timeline / marker output.

use crate::db;
use crate::error::DevError;
use crate::request::SidecarQuery;
use crate::sidecar;

/// Format a Unix epoch as a local ISO-8601 timestamp.
fn format_epoch(epoch: i64) -> String {
    // Use libc localtime_r for zero-dependency local time formatting.
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    let time_t = epoch as libc::time_t;
    unsafe { libc::localtime_r(&time_t, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}

/// Print run provenance header for sidecar queries.
///
/// Writes to stderr so it never contaminates the raw JSONL stream on
/// stdout (`--samples`, `--markers`, the default phase summary, etc.).
pub(crate) fn print_run_info(sdb: &db::sidecar::SidecarDb, uuid_prefix: &str) {
    let Some(info) = sdb.query_run_info(uuid_prefix) else {
        return;
    };
    // Only print if we have at least some provenance data.
    if info.run_start_epoch.is_none() && info.binary_path.is_none() {
        return;
    }

    // Line 1: timestamp, PID, command, mode, dataset.
    if let Some(epoch) = info.run_start_epoch {
        let dt = format_epoch(epoch);
        let mut parts = vec![format!("run {dt}")];
        if let Some(pid) = info.pid {
            parts.push(format!("PID {pid}"));
        }
        if let Some(ref cmd) = info.command {
            parts.push(format!("command: {cmd}"));
        }
        if let Some(ref mode) = info.mode {
            parts.push(format!("mode: {mode}"));
        }
        if let Some(ref dataset) = info.dataset {
            parts.push(format!("dataset: {dataset}"));
        }
        eprintln!("[sidecar] {}", parts.join("  "));
    }

    // Line 2: git commit + wall time from sidecar summary.
    let (best_idx, _) = sdb.query_meta(uuid_prefix);
    let samples = sdb.query_samples(uuid_prefix, Some(best_idx)).ok();
    let wall_time = samples.as_ref().and_then(|s| {
        if s.len() < 2 {
            return None;
        }
        let first = s.first()?.timestamp_us;
        let last = s.last()?.timestamp_us;
        let secs = (last - first) / 1_000_000;
        if secs > 0 { Some(secs) } else { None }
    });
    match (&info.git_commit, wall_time) {
        (Some(commit), Some(secs)) => {
            let (m, s) = (secs / 60, secs % 60);
            eprintln!("[sidecar] commit: {commit}  wall: {m}m{s:02}s");
        }
        (Some(commit), None) => {
            eprintln!("[sidecar] commit: {commit}");
        }
        (None, Some(secs)) => {
            let (m, s) = (secs / 60, secs % 60);
            eprintln!("[sidecar] wall: {m}m{s:02}s");
        }
        (None, None) => {}
    }

    // Line 3: exit code (only if non-zero / abnormal).
    match info.exit_code {
        Some(0) | None => {}
        Some(code) if code > 128 => {
            let sig = code - 128;
            let sig_name = match sig {
                9 => " SIGKILL (OOM?)",
                11 => " SIGSEGV",
                6 => " SIGABRT",
                _ => "",
            };
            eprintln!("[error]   exit code: {code} (signal {sig}{sig_name})");
        }
        Some(code) => {
            eprintln!("[error]   exit code: {code}");
        }
    }

    // Line 4-5: binary path and hash verification.
    if let Some(ref path) = info.binary_path {
        if let Some(ref hash) = info.binary_xxh128 {
            let short = &hash[..12.min(hash.len())];
            eprintln!("[sidecar] binary: {path} (xxh128: {short}...)");

            // Check if current binary on disk still matches.
            match crate::preflight::compute_xxh128(std::path::Path::new(path)) {
                Ok(current_hash) => {
                    if current_hash == *hash {
                        eprintln!("[sidecar] current binary xxh128: match");
                    } else {
                        let current_short = &current_hash[..12.min(current_hash.len())];
                        eprintln!(
                            "[error]   current binary differs (xxh128: {current_short}...)"
                        );
                    }
                }
                Err(_) => {
                    eprintln!("[sidecar] current binary: not found (deleted or moved)");
                }
            }
        } else {
            eprintln!("[sidecar] binary: {path}");
        }
    }
}

// ---------------------------------------------------------------------------
// Timeline query helpers
// ---------------------------------------------------------------------------

/// Resolve a phase name to a (start_us, end_us) range from markers.
///
/// Matches by:
/// 1. Exact marker name (e.g. "STAGE2_START" → that marker to the next)
/// 2. Base name (e.g. "STAGE2" → STAGE2_START to STAGE2_END)
/// 3. Substring match on marker name
pub(crate) fn resolve_phase_range(
    phase: &str,
    markers: &[sidecar::Marker],
    samples: &[sidecar::Sample],
) -> Result<(i64, i64), DevError> {
    let final_us = samples.last().map_or(0, |s| s.timestamp_us + 1);

    // Try exact match first.
    if let Some(idx) = markers.iter().position(|m| m.name == phase) {
        let start = markers[idx].timestamp_us;
        let end = markers.get(idx + 1).map_or(final_us, |m| m.timestamp_us);
        return Ok((start, end));
    }

    // Try base name: phase "STAGE2" matches "STAGE2_START".
    let start_name = format!("{phase}_START");
    let end_name = format!("{phase}_END");
    if let Some(start_idx) = markers.iter().position(|m| m.name == start_name) {
        let start = markers[start_idx].timestamp_us;
        let end = markers
            .iter()
            .find(|m| m.name == end_name)
            .map_or(final_us, |m| m.timestamp_us);
        return Ok((start, end));
    }

    // Try substring match.
    if let Some(idx) = markers.iter().position(|m| m.name.contains(phase)) {
        let start = markers[idx].timestamp_us;
        let end = markers.get(idx + 1).map_or(final_us, |m| m.timestamp_us);
        return Ok((start, end));
    }

    let available: Vec<&str> = markers.iter().map(|m| m.name.as_str()).collect();
    Err(DevError::Config(format!(
        "--phase: no marker matching '{phase}'. Available: {available:?}"
    )))
}

/// Parse a time range string like "10.0..82.0" (seconds) into (start_us, end_us).
pub(crate) fn parse_time_range(range: &str) -> Result<(i64, i64), DevError> {
    let (start_str, end_str) = range.split_once("..").ok_or_else(|| {
        DevError::Config(format!(
            "--range: expected 'start..end' in seconds, got '{range}'"
        ))
    })?;

    let start_sec: f64 = start_str.trim().parse().map_err(|_| {
        DevError::Config(format!(
            "--range: cannot parse start '{start_str}' as number"
        ))
    })?;
    let end_sec: f64 = end_str.trim().parse().map_err(|_| {
        DevError::Config(format!("--range: cannot parse end '{end_str}' as number"))
    })?;

    #[allow(clippy::cast_possible_truncation)]
    let start_us = (start_sec * 1_000_000.0) as i64;
    #[allow(clippy::cast_possible_truncation)]
    let end_us = (end_sec * 1_000_000.0) as i64;

    Ok((start_us, end_us))
}

/// All known sample field names and their accessor functions.
fn sample_field_value(s: &sidecar::Sample, field: &str) -> Option<i64> {
    match field {
        "i" => Some(i64::from(s.sample_idx)),
        "rss" => Some(s.rss_kb),
        "anon" => Some(s.anon_kb),
        "file" => Some(s.file_kb),
        "shmem" => Some(s.shmem_kb),
        "swap" => Some(s.swap_kb),
        "vsize" => Some(s.vsize_kb),
        "hwm" => Some(s.vm_hwm_kb),
        "utime" => Some(s.utime),
        "stime" => Some(s.stime),
        "threads" => Some(s.num_threads),
        "minflt" => Some(s.minflt),
        "majflt" => Some(s.majflt),
        "rchar" => Some(s.rchar),
        "wchar" => Some(s.wchar),
        "rd" => Some(s.read_bytes),
        "wr" => Some(s.write_bytes),
        "cwr" => Some(s.cancelled_write_bytes),
        "syscr" => Some(s.syscr),
        "syscw" => Some(s.syscw),
        "vcs" => Some(s.vol_cs),
        "nvcs" => Some(s.nonvol_cs),
        _ => None,
    }
}

/// Parse a --where condition like "majflt>0" or "anon>100000".
///
/// Returns (field, op, threshold). Supported ops: >, <, >=, <=, ==, !=.
fn parse_where_cond(cond: &str) -> Result<(&str, &str, i64), DevError> {
    // Try two-char operators first, then single-char.
    for op in &[">=", "<=", "==", "!=", ">", "<"] {
        if let Some(pos) = cond.find(op) {
            let field = cond[..pos].trim();
            let val_str = cond[pos + op.len()..].trim();
            let val: i64 = val_str.parse().map_err(|_| {
                DevError::Config(format!("--where: cannot parse '{val_str}' as integer"))
            })?;
            return Ok((field, op, val));
        }
    }
    Err(DevError::Config(format!(
        "--where: invalid condition '{cond}' (expected e.g. 'majflt>0')"
    )))
}

/// Apply --where, --every, --head, --tail filters to a sample list.
pub(crate) fn apply_timeline_filter<'a>(
    samples: &'a [sidecar::Sample],
    q: &SidecarQuery,
) -> Vec<&'a sidecar::Sample> {
    let mut result: Vec<&sidecar::Sample> = samples.iter().collect();

    // --where filter
    if let Some(ref cond) = q.where_cond
        && let Ok((field, op, threshold)) = parse_where_cond(cond)
    {
        result.retain(|s| {
            if let Some(val) = sample_field_value(s, field) {
                match op {
                    ">" => val > threshold,
                    "<" => val < threshold,
                    ">=" => val >= threshold,
                    "<=" => val <= threshold,
                    "==" => val == threshold,
                    "!=" => val != threshold,
                    _ => true,
                }
            } else {
                false
            }
        });
    }

    // --every N (downsample)
    if let Some(n) = q.every
        && n > 1
    {
        result = result.into_iter().step_by(n).collect();
    }

    // --tail N (take last N before head, so --tail 100 --head 10 = last 100 then first 10 of those)
    if let Some(n) = q.tail {
        let len = result.len();
        if n < len {
            result = result.split_off(len - n);
        }
    }

    // --head N
    if let Some(n) = q.head {
        result.truncate(n);
    }

    result
}

/// Print min/max/avg/p50/p95 for a field across the given samples.
pub(crate) fn print_field_stat(samples: &[&sidecar::Sample], field: &str) -> Result<(), DevError> {
    let mut values: Vec<i64> = samples
        .iter()
        .filter_map(|s| sample_field_value(s, field))
        .collect();

    if values.is_empty() {
        return Err(DevError::Config(format!(
            "unknown field '{field}' or no samples"
        )));
    }

    values.sort_unstable();
    let n = values.len();

    #[allow(clippy::cast_precision_loss)]
    let avg = values.iter().sum::<i64>() as f64 / n as f64;
    let min = values[0];
    let max = values[n - 1];

    // Linear interpolation percentiles (same as harness::percentile).
    let pct = |p: usize| -> i64 {
        if n == 1 {
            return values[0];
        }
        #[allow(clippy::cast_precision_loss)]
        let pos = (p as f64 / 100.0) * (n - 1) as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let lo = pos as usize;
        let hi = (lo + 1).min(n - 1);
        #[allow(clippy::cast_precision_loss)]
        let frac = pos - lo as f64;
        #[allow(clippy::cast_precision_loss)]
        let result = values[lo] as f64 + frac * (values[hi] - values[lo]) as f64;
        #[allow(clippy::cast_possible_truncation)]
        {
            result.round() as i64
        }
    };

    println!("field    {field}");
    println!("samples  {n}");
    println!("min      {min}");
    println!("max      {max}");
    println!("avg      {avg:.1}");
    println!("p50      {}", pct(50));
    println!("p95      {}", pct(95));
    Ok(())
}

/// Format a sample as JSONL, optionally projecting only selected fields.
///
/// `t` is output as fractional seconds (e.g. `1.234`) not microseconds.
/// When `fields` is `None`, all fields are output. When `Some`, only the
/// listed fields are included (plus `t` is always included).
pub(crate) fn sidecar_sample_json_projected(
    s: &sidecar::Sample,
    fields: Option<&Vec<String>>,
) -> String {
    // t is always fractional seconds.
    #[allow(clippy::cast_precision_loss)]
    let t_sec = s.timestamp_us as f64 / 1_000_000.0;

    match fields {
        None => {
            // All fields.
            format!(
                concat!(
                    "{{",
                    "\"t\":{:.3},",
                    "\"rss\":{},",
                    "\"anon\":{},",
                    "\"file\":{},",
                    "\"shmem\":{},",
                    "\"swap\":{},",
                    "\"vsize\":{},",
                    "\"hwm\":{},",
                    "\"utime\":{},",
                    "\"stime\":{},",
                    "\"threads\":{},",
                    "\"minflt\":{},",
                    "\"majflt\":{},",
                    "\"rchar\":{},",
                    "\"wchar\":{},",
                    "\"rd\":{},",
                    "\"wr\":{},",
                    "\"cwr\":{},",
                    "\"syscr\":{},",
                    "\"syscw\":{},",
                    "\"vcs\":{},",
                    "\"nvcs\":{}",
                    "}}",
                ),
                t_sec,
                s.rss_kb,
                s.anon_kb,
                s.file_kb,
                s.shmem_kb,
                s.swap_kb,
                s.vsize_kb,
                s.vm_hwm_kb,
                s.utime,
                s.stime,
                s.num_threads,
                s.minflt,
                s.majflt,
                s.rchar,
                s.wchar,
                s.read_bytes,
                s.write_bytes,
                s.cancelled_write_bytes,
                s.syscr,
                s.syscw,
                s.vol_cs,
                s.nonvol_cs,
            )
        }
        Some(field_list) => {
            // Projected: only requested fields + always t.
            let mut parts: Vec<String> = Vec::with_capacity(field_list.len() + 1);
            parts.push(format!("\"t\":{t_sec:.3}"));
            for f in field_list {
                if f == "t" {
                    continue; // already included
                }
                if let Some(val) = sample_field_value(s, f) {
                    parts.push(format!("\"{f}\":{val}"));
                }
            }
            format!("{{{}}}", parts.join(","))
        }
    }
}

/// Format a sidecar marker as a compact JSON object (single line).
/// `t` is fractional seconds.
pub(crate) fn sidecar_marker_json(m: &sidecar::Marker) -> String {
    #[allow(clippy::cast_precision_loss)]
    let t_sec = m.timestamp_us as f64 / 1_000_000.0;
    let name = m
        .name
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!(
        "{{\"i\":{},\"t\":{t_sec:.3},\"name\":\"{}\"}}",
        m.marker_idx, name,
    )
}

/// Print per-phase summary table from sidecar samples and markers.
///
/// Each marker defines a phase boundary. The phase runs from the marker's
/// timestamp up to (but not including) the next marker's timestamp. The
/// last phase runs to the final sample. Shows duration, peak RSS, peak
/// anon RSS, and disk I/O deltas per phase.
///
/// If there are no markers, treats the entire run as a single phase.
/// Aggregated metrics for a single phase. Computed once by
/// `compute_phase_summary`, consumed by both the JSONL and human-table
/// renderers so the two paths stay in lockstep.
struct PhaseSummary {
    name: String,
    start_us: i64,
    duration_ms: i64,
    samples: u32,
    /// Fields below are 0 when `samples == 0` (phase too short to land a
    /// sample at the 100ms cadence). Consumers treat `samples == 0` as the
    /// "no measurement" signal rather than sniffing for zero values.
    peak_rss_kb: i64,
    peak_anon_kb: i64,
    /// Largest single-sample major-fault delta observed in the phase.
    /// Zero when the phase has fewer than two in-phase samples (the
    /// delta is computed across consecutive filter-passing samples
    /// only, so the first contributes nothing).
    peak_majflt: i64,
    disk_read_kb: i64,
    disk_write_kb: i64,
    sample_span_us: i64,
    cpu_delta_jiffies: i64,
    /// Per-phase utime / stime split. `cpu_delta_jiffies` above is
    /// still the combined total (retained for compatibility with the
    /// pre-split `avg_cores` rendering); these two break it down so
    /// consumers can compute user_cores vs kernel_cores.
    utime_delta_jiffies: i64,
    stime_delta_jiffies: i64,
    /// Full per-phase deltas for page faults and context switches.
    /// `peak_majflt` above captures single-sample spikes; these are
    /// the cumulative totals across the whole phase.
    majflt_delta: i64,
    minflt_delta: i64,
    vol_cs_delta: i64,
    nonvol_cs_delta: i64,
    /// Max thread count observed across in-phase samples. Zero when
    /// `samples == 0`.
    peak_threads: i64,
}

/// Running min/max/first/last accumulator for a per-phase scalar field
/// pulled from `/proc`. `first` is set on the first in-phase sample,
/// `last` tracks every subsequent one. Delta is `last - first` clamped
/// to zero (historical pre-fix-sidecar samples can regress when the
/// process exits between /proc reads).
#[derive(Default)]
struct Running {
    first: Option<i64>,
    last: i64,
}

impl Running {
    fn observe(&mut self, v: i64) {
        if self.first.is_none() {
            self.first = Some(v);
        }
        self.last = v;
    }
    fn delta(&self) -> i64 {
        (self.last - self.first.unwrap_or(self.last)).max(0)
    }
}

fn compute_phase_summary(
    name: &str,
    start_us: i64,
    end_us: i64,
    samples: &[sidecar::Sample],
) -> PhaseSummary {
    let mut peak_rss: i64 = 0;
    let mut peak_anon: i64 = 0;
    let mut peak_majflt: i64 = 0;
    let mut peak_threads: i64 = 0;
    let mut prev_majflt: Option<i64> = None;
    let mut rd = Running::default();
    let mut wr = Running::default();
    let mut utime = Running::default();
    let mut stime = Running::default();
    let mut minflt = Running::default();
    let mut majflt_cum = Running::default();
    let mut vol_cs = Running::default();
    let mut nonvol_cs = Running::default();
    let mut ts = Running::default();
    let mut count: u32 = 0;

    for s in samples
        .iter()
        .filter(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us)
    {
        peak_rss = peak_rss.max(s.rss_kb);
        peak_anon = peak_anon.max(s.anon_kb);
        peak_threads = peak_threads.max(s.num_threads);
        if let Some(prev) = prev_majflt {
            peak_majflt = peak_majflt.max(s.majflt - prev);
        }
        prev_majflt = Some(s.majflt);
        rd.observe(s.read_bytes);
        wr.observe(s.write_bytes);
        utime.observe(s.utime);
        stime.observe(s.stime);
        minflt.observe(s.minflt);
        majflt_cum.observe(s.majflt);
        vol_cs.observe(s.vol_cs);
        nonvol_cs.observe(s.nonvol_cs);
        ts.observe(s.timestamp_us);
        count += 1;
    }

    let utime_delta = utime.delta();
    let stime_delta = stime.delta();
    PhaseSummary {
        name: name.to_owned(),
        start_us,
        duration_ms: (end_us - start_us) / 1_000,
        samples: count,
        peak_rss_kb: peak_rss,
        peak_anon_kb: peak_anon,
        peak_majflt,
        disk_read_kb: rd.delta() / 1024,
        disk_write_kb: wr.delta() / 1024,
        sample_span_us: ts.delta(),
        cpu_delta_jiffies: utime_delta + stime_delta,
        utime_delta_jiffies: utime_delta,
        stime_delta_jiffies: stime_delta,
        majflt_delta: majflt_cum.delta(),
        minflt_delta: minflt.delta(),
        vol_cs_delta: vol_cs.delta(),
        nonvol_cs_delta: nonvol_cs.delta(),
        peak_threads,
    }
}

/// Print the per-phase summary. `human = true` renders the fixed-width
/// table; the default is JSONL — one summary object then one phase object
/// per line — designed for machine/LLM consumption.
pub(crate) fn print_phase_summary(
    samples: &[sidecar::Sample],
    markers: &[sidecar::Marker],
    human: bool,
) {
    // Shared with `--compare`: pairs `*_START`/`*_END` into a single
    // phase rather than treating each marker as an independent boundary.
    let phases = build_phases(markers, samples);
    let clk_tck = clk_tck_per_second();

    let summaries: Vec<PhaseSummary> = phases
        .iter()
        .map(|(name, start, end)| compute_phase_summary(name, *start, *end, samples))
        .collect();

    if human {
        print_phase_summary_human(&summaries, clk_tck);
    } else {
        print_phase_summary_jsonl(&summaries, samples, clk_tck);
    }
}

fn print_phase_summary_human(summaries: &[PhaseSummary], clk_tck: i64) {
    println!(
        "{:<24} {:>8} {:>10} {:>10} {:>10} {:>12} {:>12} {:>10}",
        "Phase",
        "Duration",
        "Peak RSS",
        "Peak Anon",
        "Peak Mflt",
        "Disk Read",
        "Disk Write",
        "Avg Cores",
    );

    for s in summaries {
        if s.samples == 0 {
            // Phase is shorter than the 100ms sampling interval (or landed
            // entirely between two sample ticks). Still show the marker
            // duration — zero samples is a real signal, not missing data.
            println!("{:<24} {:>6}ms (no samples)", s.name, s.duration_ms);
            continue;
        }
        let avg_cores = avg_cores_str(s.cpu_delta_jiffies, s.sample_span_us, clk_tck);
        println!(
            "{:<24} {:>6}ms {:>7} kB {:>7} kB {:>10} {:>9} kB {:>9} kB {:>10}",
            s.name,
            s.duration_ms,
            s.peak_rss_kb,
            s.peak_anon_kb,
            s.peak_majflt,
            s.disk_read_kb,
            s.disk_write_kb,
            avg_cores,
        );
        // Continuation line: user/kernel core split + fault/ctxt deltas +
        // peak thread count. Kept indented to read as a per-phase
        // detail, not a separate phase. Field labels are inline so
        // grep-style inspection stays workable.
        let user_cores = avg_cores_str(s.utime_delta_jiffies, s.sample_span_us, clk_tck);
        let kernel_cores = avg_cores_str(s.stime_delta_jiffies, s.sample_span_us, clk_tck);
        println!(
            "{:<24}   user={user_cores} kern={kernel_cores} majflt={} minflt={} vol_cs={} nonvol_cs={} peak_threads={}",
            "",
            s.majflt_delta,
            s.minflt_delta,
            s.vol_cs_delta,
            s.nonvol_cs_delta,
            s.peak_threads,
        );
    }
}

fn print_phase_summary_jsonl(
    summaries: &[PhaseSummary],
    samples: &[sidecar::Sample],
    clk_tck: i64,
) {
    // Top-level summary record: how many phases, total wall time, peak
    // thread count observed across the whole run. Always emitted first so
    // a streaming parser can allocate based on `phases`.
    let wall_ms = samples
        .first()
        .zip(samples.last())
        .map(|(a, b)| (b.timestamp_us - a.timestamp_us) / 1_000)
        .unwrap_or(0);
    let peak_threads = samples.iter().map(|s| s.num_threads).max().unwrap_or(0);
    let header = serde_json::json!({
        "type": "summary",
        "phases": summaries.len(),
        "wall_ms": wall_ms,
        "peak_threads": peak_threads,
    });
    println!("{header}");

    for s in summaries {
        // `avg_cores` is `null` (not 0) when we couldn't measure — zero is
        // ambiguous with "truly idle". Same for the memory/io fields, which
        // we simply omit when samples == 0.
        let obj = if s.samples == 0 {
            serde_json::json!({
                "type": "phase",
                "name": s.name,
                "start_us": s.start_us,
                "duration_ms": s.duration_ms,
                "samples": 0,
                "avg_cores": serde_json::Value::Null,
            })
        } else {
            let avg_cores = avg_cores_f64(s.cpu_delta_jiffies, s.sample_span_us, clk_tck);
            let user_cores = avg_cores_f64(s.utime_delta_jiffies, s.sample_span_us, clk_tck);
            let kernel_cores = avg_cores_f64(s.stime_delta_jiffies, s.sample_span_us, clk_tck);
            serde_json::json!({
                "type": "phase",
                "name": s.name,
                "start_us": s.start_us,
                "duration_ms": s.duration_ms,
                "samples": s.samples,
                "peak_rss_kb": s.peak_rss_kb,
                "peak_anon_kb": s.peak_anon_kb,
                "peak_majflt": s.peak_majflt,
                "disk_read_kb": s.disk_read_kb,
                "disk_write_kb": s.disk_write_kb,
                "sample_span_us": s.sample_span_us,
                "cpu_delta_jiffies": s.cpu_delta_jiffies,
                "avg_cores": avg_cores,
                "user_cores": user_cores,
                "kernel_cores": kernel_cores,
                "majflt_delta": s.majflt_delta,
                "minflt_delta": s.minflt_delta,
                "vol_cs_delta": s.vol_cs_delta,
                "nonvol_cs_delta": s.nonvol_cs_delta,
                "peak_threads": s.peak_threads,
            })
        };
        println!("{obj}");
    }
}

/// Print duration between _START/_END marker pairs.
///
/// Matches markers by stripping the `_START`/`_END` suffix to find pairs.
/// For unpaired markers (standalone), prints the timestamp only. JSONL by
/// default; `human` renders the fixed-width table.
pub(crate) fn print_marker_durations(markers: &[sidecar::Marker], human: bool) {
    // Build a map of base_name -> (start_us, end_us).
    let mut pairs: Vec<(String, i64, Option<i64>)> = Vec::new();

    // Index of consumed markers (to avoid double-counting).
    let mut consumed = vec![false; markers.len()];

    for (i, m) in markers.iter().enumerate() {
        if consumed[i] {
            continue;
        }
        if let Some(base) = m.name.strip_suffix("_START") {
            consumed[i] = true;
            let end_name = format!("{base}_END");
            // Find the matching END.
            let end_us = markers[i + 1..]
                .iter()
                .enumerate()
                .find(|(_, m2)| m2.name == end_name)
                .map(|(j, m2)| {
                    consumed[i + 1 + j] = true;
                    m2.timestamp_us
                });
            pairs.push((base.to_owned(), m.timestamp_us, end_us));
        }
    }

    // Print standalone markers that weren't consumed.
    let mut standalone: Vec<&sidecar::Marker> = Vec::new();
    for (i, m) in markers.iter().enumerate() {
        if !consumed[i] {
            standalone.push(m);
        }
    }

    if human {
        print_marker_durations_human(&pairs, &standalone);
    } else {
        print_marker_durations_jsonl(&pairs, &standalone);
    }
}

fn print_marker_durations_jsonl(
    pairs: &[(String, i64, Option<i64>)],
    standalone: &[&sidecar::Marker],
) {
    for (name, start_us, end_us) in pairs {
        let obj = match end_us {
            Some(end) => serde_json::json!({
                "type": "phase",
                "name": name,
                "start_us": start_us,
                "end_us": end,
                "duration_ms": (end - start_us) / 1_000,
            }),
            None => serde_json::json!({
                "type": "phase",
                "name": name,
                "start_us": start_us,
                "end_us": serde_json::Value::Null,
                "duration_ms": serde_json::Value::Null,
            }),
        };
        println!("{obj}");
    }
    for m in standalone {
        let obj = serde_json::json!({
            "type": "standalone",
            "name": m.name,
            "timestamp_us": m.timestamp_us,
        });
        println!("{obj}");
    }
}

fn print_marker_durations_human(
    pairs: &[(String, i64, Option<i64>)],
    standalone: &[&sidecar::Marker],
) {
    if !pairs.is_empty() {
        println!(
            "{:<32} {:>12} {:>12} {:>12}",
            "Phase", "Start", "End", "Duration"
        );
        println!("{}", "-".repeat(71));
        for (name, start_us, end_us) in pairs {
            match end_us {
                Some(end) => {
                    let dur_ms = (end - start_us) / 1_000;
                    let start_ms = start_us / 1_000;
                    let end_ms = end / 1_000;
                    println!("{name:<32} {start_ms:>9}ms {end_ms:>9}ms {dur_ms:>9}ms");
                }
                None => {
                    let start_ms = start_us / 1_000;
                    println!(
                        "{name:<32} {:>9}ms {:>12} {:>12}",
                        start_ms, "(no end)", "—"
                    );
                }
            }
        }
    }

    if !standalone.is_empty() {
        if !pairs.is_empty() {
            println!();
        }
        println!("Standalone markers:");
        for m in standalone {
            let ms = m.timestamp_us / 1_000;
            println!("  {:<32} {:>9}ms", m.name, ms);
        }
    }
}

/// Print phase-aligned comparison of two sidecar timelines. Default is
/// JSONL (`{"type":"compare","name":...,"a":{...},"b":{...},"delta_pct":...}`);
/// `human = true` gives the fixed-width table.
pub(crate) fn print_compare_timeline(
    uuid_a: &str,
    samples_a: &[sidecar::Sample],
    markers_a: &[sidecar::Marker],
    uuid_b: &str,
    samples_b: &[sidecar::Sample],
    markers_b: &[sidecar::Marker],
    human: bool,
) {
    // Build phases from run A's markers (or all markers if A has none).
    let phases_a = build_phases(markers_a, samples_a);
    let phases_b = build_phases(markers_b, samples_b);
    let clk_tck = clk_tck_per_second();

    if human {
        print_compare_timeline_human(
            uuid_a, samples_a, &phases_a, samples_b, &phases_b, uuid_b, clk_tck,
        );
    } else {
        print_compare_timeline_jsonl(
            uuid_a, samples_a, &phases_a, samples_b, &phases_b, uuid_b, clk_tck,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn print_compare_timeline_human(
    uuid_a: &str,
    samples_a: &[sidecar::Sample],
    phases_a: &[(String, i64, i64)],
    samples_b: &[sidecar::Sample],
    phases_b: &[(String, i64, i64)],
    uuid_b: &str,
    clk_tck: i64,
) {
    let short_a = &uuid_a[..8.min(uuid_a.len())];
    let short_b = &uuid_b[..8.min(uuid_b.len())];

    println!(
        "{:<20} {:>30} {:>30} {:>8}",
        "Phase",
        format!("Run A ({short_a})"),
        format!("Run B ({short_b})"),
        "Delta",
    );

    for (name, start_a, end_a) in phases_a {
        let stats_a = phase_stats(samples_a, *start_a, *end_a);
        let avg_cores_a = avg_cores_str(stats_a.cpu_delta_jiffies, stats_a.sample_span_us, clk_tck);

        let match_b = phases_b
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, start, end)| (phase_stats(samples_b, *start, *end), *start, *end));

        let dur_a = (end_a - start_a) / 1_000;

        match match_b {
            Some((sb, sb_start, sb_end)) => {
                let dur_b = (sb_end - sb_start) / 1_000;
                let avg_cores_b = avg_cores_str(sb.cpu_delta_jiffies, sb.sample_span_us, clk_tck);

                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let delta_pct = if dur_a > 0 {
                    ((dur_b - dur_a) as f64 / dur_a as f64 * 100.0) as i64
                } else {
                    0
                };

                println!(
                    "{:<20} {:>5}ms {:>6}kB {:>5}MB {:>5}c  {:>5}ms {:>6}kB {:>5}MB {:>5}c  {:>+5}%",
                    name,
                    dur_a,
                    stats_a.peak_anon,
                    stats_a.disk_read_kb / 1024,
                    avg_cores_a,
                    dur_b,
                    sb.peak_anon,
                    sb.disk_read_kb / 1024,
                    avg_cores_b,
                    delta_pct,
                );
            }
            None => {
                println!(
                    "{:<20} {:>5}ms {:>6}kB {:>5}MB {:>5}c  {:>30} {:>8}",
                    name,
                    dur_a,
                    stats_a.peak_anon,
                    stats_a.disk_read_kb / 1024,
                    avg_cores_a,
                    "(no match)",
                    "—",
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn print_compare_timeline_jsonl(
    uuid_a: &str,
    samples_a: &[sidecar::Sample],
    phases_a: &[(String, i64, i64)],
    samples_b: &[sidecar::Sample],
    phases_b: &[(String, i64, i64)],
    uuid_b: &str,
    clk_tck: i64,
) {
    let header = serde_json::json!({
        "type": "compare_summary",
        "a_uuid": uuid_a,
        "b_uuid": uuid_b,
        "phases": phases_a.len(),
    });
    println!("{header}");

    for (name, start_a, end_a) in phases_a {
        let stats_a = phase_stats(samples_a, *start_a, *end_a);
        let match_b = phases_b
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, start, end)| (phase_stats(samples_b, *start, *end), *start, *end));

        let dur_a_ms = (end_a - start_a) / 1_000;
        let a_obj = compare_side_json(&stats_a, dur_a_ms, clk_tck);

        let (b_obj, delta_pct) = match match_b {
            Some((sb, sb_start, sb_end)) => {
                let dur_b_ms = (sb_end - sb_start) / 1_000;
                #[allow(clippy::cast_precision_loss)]
                let pct = if dur_a_ms > 0 {
                    let raw = (dur_b_ms - dur_a_ms) as f64 / dur_a_ms as f64 * 100.0;
                    // Round to 2 decimals — same rationale as avg_cores.
                    Some((raw * 100.0).round() / 100.0)
                } else {
                    None
                };
                (compare_side_json(&sb, dur_b_ms, clk_tck), pct)
            }
            None => (serde_json::Value::Null, None),
        };

        let obj = serde_json::json!({
            "type": "compare_phase",
            "name": name,
            "a": a_obj,
            "b": b_obj,
            "delta_pct": delta_pct,
        });
        println!("{obj}");
    }
}

fn compare_side_json(stats: &PhaseStats, duration_ms: i64, clk_tck: i64) -> serde_json::Value {
    serde_json::json!({
        "duration_ms": duration_ms,
        "peak_anon_kb": stats.peak_anon,
        "disk_read_kb": stats.disk_read_kb,
        "cpu_delta_jiffies": stats.cpu_delta_jiffies,
        "sample_span_us": stats.sample_span_us,
        "avg_cores": avg_cores_f64(stats.cpu_delta_jiffies, stats.sample_span_us, clk_tck),
    })
}

struct PhaseStats {
    peak_anon: i64,
    disk_read_kb: i64,
    /// CPU jiffies accumulated between the first and last sample inside the
    /// phase. Paired with `sample_span_us` for `avg_cores_str`.
    cpu_delta_jiffies: i64,
    /// Wall-clock microseconds from the first to the last sample inside the
    /// phase. Zero if fewer than two samples landed in the phase.
    sample_span_us: i64,
}

/// `sysconf(_SC_CLK_TCK)` — jiffies per second used to decode
/// `/proc/<pid>/stat`'s `utime`/`stime`. Always 100 on typical Linux
/// x86_64, but the kernel can be built with 250, 300, or 1000; read it
/// at runtime so we stay correct on arbitrary hosts.
fn clk_tck_per_second() -> i64 {
    // SAFETY: `sysconf` is a plain C function with no aliasing or
    // memory-safety implications. Returns -1 if the name isn't
    // supported; we clamp to 100 (the usual default) in that case.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v <= 0 { 100 } else { v }
}

/// Format the average-cores-used figure over a phase. Takes the CPU
/// jiffy delta (`utime + stime` at phase end minus at phase start),
/// the wall-time delta in microseconds, and the system's clock-tick
/// frequency. Returns a short string like `"3.1"` or `"—"` when the
/// phase is too short for a stable measurement.
fn avg_cores_str(cpu_delta_jiffies: i64, wall_us: i64, clk_tck: i64) -> String {
    avg_cores_f64(cpu_delta_jiffies, wall_us, clk_tck)
        .map_or_else(|| "—".to_owned(), |c| format!("{c:.1}"))
}

/// Numeric core of `avg_cores_str`. Returns `None` when the sample span is
/// too short (or degenerate) to give a stable reading — callers pick how
/// to render: "—" for the table, `null` for JSON.
///
/// Result is rounded to 2 decimal places so JSON consumers aren't staring
/// at f64 precision artefacts like `0.36274423029811576`.
fn avg_cores_f64(cpu_delta_jiffies: i64, wall_us: i64, clk_tck: i64) -> Option<f64> {
    if wall_us <= 0 || clk_tck <= 0 || cpu_delta_jiffies < 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let cpu_secs = cpu_delta_jiffies as f64 / clk_tck as f64;
    #[allow(clippy::cast_precision_loss)]
    let wall_secs = wall_us as f64 / 1_000_000.0;
    Some(((cpu_secs / wall_secs) * 100.0).round() / 100.0)
}

fn phase_stats(samples: &[sidecar::Sample], start_us: i64, end_us: i64) -> PhaseStats {
    let mut peak_anon: i64 = 0;
    let mut first_rd: Option<i64> = None;
    let mut last_rd: i64 = 0;
    let mut first_cpu: Option<i64> = None;
    let mut last_cpu: i64 = 0;
    let mut first_ts: Option<i64> = None;
    let mut last_ts: i64 = 0;

    for s in samples
        .iter()
        .filter(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us)
    {
        if s.anon_kb > peak_anon {
            peak_anon = s.anon_kb;
        }
        if first_rd.is_none() {
            first_rd = Some(s.read_bytes);
        }
        last_rd = s.read_bytes;
        let cpu = s.utime + s.stime;
        if first_cpu.is_none() {
            first_cpu = Some(cpu);
        }
        last_cpu = cpu;
        if first_ts.is_none() {
            first_ts = Some(s.timestamp_us);
        }
        last_ts = s.timestamp_us;
    }

    PhaseStats {
        peak_anon,
        // Clamp negative deltas (historical pre-fix-sidecar samples could
        // regress to zero when the process exited between /proc reads).
        disk_read_kb: ((last_rd - first_rd.unwrap_or(0)).max(0)) / 1024,
        cpu_delta_jiffies: last_cpu - first_cpu.unwrap_or(last_cpu),
        sample_span_us: last_ts - first_ts.unwrap_or(last_ts),
    }
}

/// Build phase boundaries from markers (or single "(all)" phase if no markers).
/// Split the sample stream into inter-marker segments.
///
/// Markers are point-in-time bookmarks in the stream (see the FIFO protocol
/// in pbfhogg's `emit_marker` — timestamp + name, nothing else). A segment
/// runs from marker N to marker N+1; the last segment runs from the final
/// marker to end-of-samples. The segment is labelled with the name of the
/// marker that opens it.
///
/// We deliberately DON'T interpret any naming convention (`_START`/`_END`)
/// as span structure. If you want duration between paired markers, use
/// `print_marker_durations` (`--durations`), which is explicitly opt-in
/// about the pairing.
fn build_phases(
    markers: &[sidecar::Marker],
    samples: &[sidecar::Sample],
) -> Vec<(String, i64, i64)> {
    let mut phases = Vec::new();
    if markers.is_empty() {
        if let (Some(first), Some(last)) = (samples.first(), samples.last()) {
            phases.push(("(all)".to_owned(), first.timestamp_us, last.timestamp_us + 1));
        }
        return phases;
    }

    let final_us = samples.last().map_or(0, |s| s.timestamp_us + 1);
    for (i, m) in markers.iter().enumerate() {
        let phase_end = markers
            .get(i + 1)
            .map_or(final_us, |next| next.timestamp_us);
        phases.push((m.name.clone(), m.timestamp_us, phase_end));
    }
    phases
}

/// Sum `WAIT_<CATEGORY>_START`/`_END` pair durations by category and
/// render as total ms + fraction of run wall-clock. Runs that predate
/// the convention — no `WAIT_*` markers at all — print a clear message
/// pointing at the conventions section of the README so users don't
/// confuse "nothing instrumented" with "no stalls observed."
///
/// Pairing reuses the same `_START`/`_END` logic as `--durations`.
/// Unpaired `WAIT_*_START` markers are silently ignored (consistent
/// with `--durations` treating them as "no duration known"). We could
/// surface them as truncated-stall hints but they're rare enough in
/// practice to not justify the noise.
pub(crate) fn print_stalls(
    markers: &[sidecar::Marker],
    wall_us: i64,
    human: bool,
) {
    use std::collections::BTreeMap;

    let mut totals_us: BTreeMap<String, i64> = BTreeMap::new();
    let mut consumed = vec![false; markers.len()];

    for (i, m) in markers.iter().enumerate() {
        if consumed[i] {
            continue;
        }
        let Some(base) = m.name.strip_suffix("_START") else {
            continue;
        };
        let Some(category) = base.strip_prefix("WAIT_") else {
            continue;
        };
        consumed[i] = true;
        let end_name = format!("{base}_END");
        let end = markers[i + 1..]
            .iter()
            .enumerate()
            .find(|(_, m2)| m2.name == end_name);
        if let Some((j, end_m)) = end {
            consumed[i + 1 + j] = true;
            let dur_us = end_m.timestamp_us - m.timestamp_us;
            *totals_us.entry(category.to_owned()).or_insert(0) += dur_us.max(0);
        }
    }

    if totals_us.is_empty() {
        crate::output::result_msg(
            "no WAIT_* marker pairs in this run — see the \"Sidecar conventions\" \
             section of brokkr's README for the stall-attribution convention",
        );
        return;
    }

    if human {
        print_stalls_human(&totals_us, wall_us);
    } else {
        print_stalls_jsonl(&totals_us, wall_us);
    }
}

fn print_stalls_human(totals_us: &std::collections::BTreeMap<String, i64>, wall_us: i64) {
    println!("{:<24} {:>12} {:>10}", "Category", "Total", "% of wall");
    println!("{}", "-".repeat(48));
    let mut entries: Vec<(&String, &i64)> = totals_us.iter().collect();
    entries.sort_by(|a, b| b.1.cmp(a.1));
    for (cat, total_us) in entries {
        let ms = total_us / 1_000;
        let pct = wall_pct(*total_us, wall_us);
        let pct_str = pct.map_or_else(|| "-".to_owned(), |p| format!("{p:>6.2}%"));
        println!("{cat:<24} {ms:>9}ms {pct_str:>10}");
    }
}

fn print_stalls_jsonl(totals_us: &std::collections::BTreeMap<String, i64>, wall_us: i64) {
    for (cat, total_us) in totals_us {
        let obj = serde_json::json!({
            "type": "stall",
            "category": cat,
            "total_ms": total_us / 1_000,
            "total_us": total_us,
            "wall_us": wall_us,
            "wall_fraction": wall_pct(*total_us, wall_us).map(|p| p / 100.0),
        });
        println!("{obj}");
    }
}

#[allow(clippy::cast_precision_loss)]
fn wall_pct(numerator_us: i64, wall_us: i64) -> Option<f64> {
    if wall_us <= 0 {
        return None;
    }
    Some(numerator_us as f64 / wall_us as f64 * 100.0)
}

/// Print START/END marker pairs with duration + peak RSS and majflt from samples.
/// Print counters as a simple list.
pub(crate) fn print_counters(counters: &[sidecar::Counter], human: bool) {
    if human {
        for c in counters {
            #[allow(clippy::cast_precision_loss)]
            let t_sec = c.timestamp_us as f64 / 1_000_000.0;
            println!("t={t_sec:<10.3} {}={}", c.name, c.value);
        }
        return;
    }
    for c in counters {
        #[allow(clippy::cast_precision_loss)]
        let t_sec = c.timestamp_us as f64 / 1_000_000.0;
        let obj = serde_json::json!({
            "t": t_sec,
            "name": c.name,
            "value": c.value,
        });
        println!("{obj}");
    }
}

