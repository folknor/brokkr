//! Monitoring sidecar that runs alongside benchmark processes.
//!
//! Samples `/proc/{pid}/*` at fixed intervals, reads application phase markers
//! from a FIFO, and hands everything back to the harness, which stores it in
//! SQLite after the child exits. The only write during the benchmark itself
//! is the status file below.
//!
//! # Status file
//!
//! Each drain that saw a phase marker writes
//! the latest marker name to the status file ([`status_path`]) so `brokkr
//! lock` can show it. The file has one fixed location beside the global lock,
//! derived by one function that both the writer and the reader call - it used
//! to sit beside the FIFO, whose directory varies by harness path, while the
//! reader looked in `<project>/.brokkr/`, so every path whose FIFO lived
//! elsewhere (`--hotpath`/`--alloc`, the ratatoskr sync bench) was invisible.
//!
//! # Deadlines
//!
//! [`run_sidecar_with_deadline`] kills the child's process group once it has
//! run longer than the given limit, and the run records `Ending::Deadline`.
//!
//! # Pipe draining
//!
//! The child's stdout and stderr are piped (for error reporting). To avoid
//! deadlock when the child fills the OS pipe buffer (~64 KiB), we take
//! ownership of the pipe handles and drain them in background threads while
//! the shared child supervisor polls. A sidecar observer supplies `/proc`
//! samples and marker checks on each supervisor tick.
//! After the child exits, drains get a bounded grace period to reach EOF. If
//! a descendant still holds a pipe open, capture is stopped and its end is
//! closed so the sidecar cannot hang waiting for that descendant.
//!
//! # Timing
//!
//! `Instant::now()` provides sample timestamps and drift-free tick scheduling
//! through the shared child supervisor. The child's wall-clock time is
//! captured when the supervisor observes the ending, before pipe drains settle.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use crate::error::DevError;
use crate::output::child::{self, ChildLimit, Ending};
use crate::output::console;
use crate::process::{Pid, ProcStat};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Environment variable the measured child uses to find the marker FIFO.
/// This name is part of the sidecar emitter protocol.
pub(crate) const MARKER_FIFO_ENV: &str = "BROKKR_MARKER_FIFO";

/// Measurement defaults shared by the sidecar, harness output handling and
/// sidecar file naming. Callers may copy this value and override fields in
/// focused tests or alternate measurement runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MeasurementTunables {
    pub sample_interval: Duration,
    pub sidecar_backup_copies: usize,
    pub stderr_preview_chars: usize,
    pub hotpath_report_filename: &'static str,
    /// Maximum profile samples retained across every run in one measurement.
    pub max_samples: usize,
    /// Maximum phase markers retained across every run in one measurement.
    pub max_markers: usize,
    /// Maximum application counters retained across every run in one measurement.
    pub max_counters: usize,
    /// Maximum bytes retained from one FIFO protocol line.
    pub max_fifo_line_bytes: usize,
}

pub(crate) const DEFAULT_MEASUREMENT_TUNABLES: MeasurementTunables = MeasurementTunables {
    sample_interval: Duration::from_micros(100_000),
    // A bounded recovery history: the current snapshot plus two prior copies.
    sidecar_backup_copies: 3,
    stderr_preview_chars: 500,
    hotpath_report_filename: "hotpath-report.json",
    max_samples: 100_000,
    max_markers: 10_000,
    max_counters: 20_000,
    max_fifo_line_bytes: 1_024,
};

/// Compute a percentile from a sorted sample using linear interpolation.
/// Benchmark and sidecar summaries share this calculation so they cannot
/// disagree for the same sample set.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn linear_percentile(sorted: &[i64], pct: usize) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let len = sorted.len();
    if len == 1 {
        return sorted[0];
    }
    #[allow(clippy::cast_precision_loss)]
    let pos = (pct as f64 / 100.0) * (len - 1) as f64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let lo = pos as usize;
    let hi = (lo + 1).min(len - 1);
    #[allow(clippy::cast_precision_loss)]
    let frac = pos - lo as f64;
    #[allow(clippy::cast_precision_loss)]
    let result = sorted[lo] as f64 + frac * (sorted[hi] - sorted[lo]) as f64;
    result.round() as i64
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One sidecar sample field, with its public CLI/JSON name.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SampleField {
    pub name: &'static str,
}

/// Define the sample shape and all derived field/storage views together.
/// The aliases are the public `--fields`/JSON names; the Rust names are also
/// the SQLite column names. Keeping these pairs here lets the database and
/// formatter derive their projections from the same list.
macro_rules! define_sample {
    ($($field:ident => $name:literal;)+) => {
        /// A single point-in-time sample of process metrics from `/proc`.
        #[derive(Debug, Clone)]
        pub struct Sample {
            pub sample_idx: i32,
            pub timestamp_us: i64,
            $(pub $field: i64,)*
        }

        pub(crate) const SAMPLE_FIELDS: &[SampleField] = &[
            SampleField { name: "i" },
            $(SampleField { name: $name },)*
        ];

        impl Sample {
            pub(crate) fn field_value(&self, name: &str) -> Option<i64> {
                match name {
                    "i" => Some(i64::from(self.sample_idx)),
                    $($name => Some(self.$field),)*
                    _ => None,
                }
            }

            pub(crate) fn db_column_names() -> Vec<&'static str> {
                vec!["sample_idx", "timestamp_us", $(stringify!($field),)*]
            }

            pub(crate) fn db_column_definitions() -> Vec<String> {
                vec![
                    "sample_idx INTEGER NOT NULL".to_owned(),
                    "timestamp_us INTEGER NOT NULL".to_owned(),
                    $(format!("{} INTEGER", stringify!($field)),)*
                ]
            }

            pub(crate) fn db_values(&self) -> Vec<rusqlite::types::Value> {
                vec![
                    rusqlite::types::Value::Integer(i64::from(self.sample_idx)),
                    rusqlite::types::Value::Integer(self.timestamp_us),
                    $(rusqlite::types::Value::Integer(self.$field),)*
                ]
            }

            pub(crate) fn from_db_row(
                row: &rusqlite::Row<'_>,
            ) -> rusqlite::Result<Self> {
                Ok(Self {
                    sample_idx: row.get("sample_idx")?,
                    timestamp_us: row.get("timestamp_us")?,
                    $($field: row.get(stringify!($field))?,)*
                })
            }
        }
    };
}

define_sample! {
    rss_kb => "rss";
    anon_kb => "anon";
    file_kb => "file";
    shmem_kb => "shmem";
    swap_kb => "swap";
    vsize_kb => "vsize";
    vm_hwm_kb => "hwm";
    utime => "utime";
    stime => "stime";
    num_threads => "threads";
    minflt => "minflt";
    majflt => "majflt";
    rchar => "rchar";
    wchar => "wchar";
    read_bytes => "rd";
    write_bytes => "wr";
    cancelled_write_bytes => "cwr";
    syscr => "syscr";
    syscw => "syscw";
    vol_cs => "vcs";
    nonvol_cs => "nvcs";
}

/// A phase marker emitted by the target process.
#[derive(Debug, Clone)]
pub struct Marker {
    pub marker_idx: i32,
    pub timestamp_us: i64,
    pub name: String,
}

/// A completed span derived from a matching `_START` and later `_END` marker.
#[derive(Debug, Clone)]
pub(crate) struct TimelineSpan {
    pub name: String,
    pub start_us: i64,
    pub end_us: i64,
}

/// A segment between adjacent markers, used by phase-oriented views.
#[derive(Debug, Clone)]
pub(crate) struct TimelineSegment {
    pub name: String,
    pub start_us: i64,
    pub end_us: i64,
}

/// A single interpretation surface for the marker stream.
///
/// The FIFO protocol remains a sequence of point markers. This model exposes
/// the documented projections over that sequence: adjacent-marker segments,
/// `_START`/`_END` spans, and phase-name lookup.
pub(crate) struct Timeline<'a> {
    markers: &'a [Marker],
    final_us: i64,
    first_sample_us: Option<i64>,
}

impl<'a> Timeline<'a> {
    /// Build a timeline for a stored run, using its sampled end as the final
    /// boundary and retaining the whole-run fallback for markerless runs.
    pub(crate) fn new(markers: &'a [Marker], samples: &[Sample]) -> Self {
        Self {
            markers,
            final_us: samples.last().map_or(0, |s| s.timestamp_us + 1),
            first_sample_us: samples.first().map(|s| s.timestamp_us),
        }
    }

    /// Inter-marker segments. Every marker opens a segment ending at the next
    /// marker; the last one ends at the final sample. With no markers, the
    /// sampled run is one `(all)` segment.
    pub(crate) fn segments(&self) -> Vec<TimelineSegment> {
        if self.markers.is_empty() {
            return self.first_sample_us.map_or_else(Vec::new, |start_us| {
                vec![TimelineSegment {
                    name: "(all)".to_owned(),
                    start_us,
                    end_us: self.final_us,
                }]
            });
        }

        self.markers
            .iter()
            .enumerate()
            .map(|(i, marker)| TimelineSegment {
                name: marker.name.clone(),
                start_us: marker.timestamp_us,
                end_us: self
                    .markers
                    .get(i + 1)
                    .map_or(self.final_us, |next| next.timestamp_us),
            })
            .collect()
    }

    /// Derive completed `_START`/`_END` spans and the point markers that do
    /// not participate in one. Unmatched starts remain standalone points.
    pub(crate) fn durations(&self) -> (Vec<TimelineSpan>, Vec<&'a Marker>) {
        let mut spans = Vec::new();
        let mut consumed = vec![false; self.markers.len()];

        for (i, marker) in self.markers.iter().enumerate() {
            if consumed[i] {
                continue;
            }
            let Some(base) = marker.name.strip_suffix("_START") else {
                continue;
            };

            let end_name = format!("{base}_END");
            if let Some((j, candidate)) = self.markers[i + 1..]
                .iter()
                .enumerate()
                .find(|(j, candidate)| !consumed[i + 1 + *j] && candidate.name == end_name)
            {
                consumed[i] = true;
                consumed[i + 1 + j] = true;
                let end_us = candidate.timestamp_us;
                spans.push(TimelineSpan {
                    name: base.to_owned(),
                    start_us: marker.timestamp_us,
                    end_us,
                });
            }
        }

        let standalone = self
            .markers
            .iter()
            .enumerate()
            .filter_map(|(i, marker)| (!consumed[i]).then_some(marker))
            .collect();
        (spans, standalone)
    }

    /// Resolve `--phase`: exact event segment, then a base-name span, then a
    /// substring event segment.
    pub(crate) fn resolve_phase_range(&self, phase: &str) -> Option<(i64, i64)> {
        if let Some(index) = self.markers.iter().position(|m| m.name == phase) {
            let marker = &self.markers[index];
            let end = self
                .markers
                .get(index + 1)
                .map_or(self.final_us, |next| next.timestamp_us);
            return Some((marker.timestamp_us, end));
        }

        if let Some(span) = self
            .durations()
            .0
            .into_iter()
            .find(|span| span.name == phase)
        {
            return Some((span.start_us, span.end_us));
        }

        self.markers
            .iter()
            .position(|m| m.name.contains(phase))
            .map(|index| {
                let marker = &self.markers[index];
                let end = self
                    .markers
                    .get(index + 1)
                    .map_or(self.final_us, |next| next.timestamp_us);
                (marker.timestamp_us, end)
            })
    }

    pub(crate) fn marker_names(&self) -> Vec<&str> {
        self.markers
            .iter()
            .map(|marker| marker.name.as_str())
            .collect()
    }
}

/// An application-level counter emitted by the target process.
#[derive(Debug, Clone)]
pub struct Counter {
    pub timestamp_us: i64,
    pub name: String,
    pub value: i64,
}

/// Summary of a sidecar run.
#[derive(Debug, Clone)]
pub struct SidecarSummary {
    pub vm_hwm_kb: i64,
    pub sample_count: i32,
    pub marker_count: i32,
    pub wall_time_ms: i64,
    /// A configured storage or child-output limit omitted samples, markers,
    /// counters, FIFO records, or stdout/stderr bytes.
    pub capture_truncated: bool,
}

/// Complete sidecar data for one run of a benchmark.
pub struct SidecarData {
    pub samples: Vec<Sample>,
    pub markers: Vec<Marker>,
    pub counters: Vec<Counter>,
    /// Marker that actually ended the child, if a configured stop marker hit.
    /// This is `None` when the child exited naturally, even if a stop was
    /// requested.
    pub fired_stop_marker: Option<String>,
    /// The termination cause assigned by the child supervisor.
    pub ending: Ending,
    pub summary: SidecarSummary,
}

/// Result of a sidecar-monitored child process run.
///
/// Contains bounded captured output with per-stream completeness flags, the
/// child's ending and wall-clock time, and the sidecar profile data.
pub struct SidecarRunResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub output_cut: bool,
    pub elapsed: Duration,
    pub data: SidecarData,
}

// ---------------------------------------------------------------------------
// /proc parsing
// ---------------------------------------------------------------------------

/// Fields extracted from `/proc/{pid}/io`.
struct ProcIo {
    rchar: i64,
    wchar: i64,
    read_bytes: i64,
    write_bytes: i64,
    cancelled_write_bytes: i64,
    syscr: i64,
    syscw: i64,
}

/// Fields extracted from `/proc/{pid}/status`.
struct ProcStatus {
    vm_rss_kb: i64,
    rss_anon_kb: i64,
    rss_file_kb: i64,
    rss_shmem_kb: i64,
    vm_swap_kb: i64,
    vm_hwm_kb: i64,
    vol_cs: i64,
    nonvol_cs: i64,
}

/// Read `/proc/{pid}/stat` through the shared process parser.
fn read_proc_stat(pid: u32) -> Option<ProcStat> {
    let pid = Pid::new(pid)?;
    let path = format!("/proc/{}/stat", pid.get());
    let contents = fs::read_to_string(path).ok()?;
    let stat = parse_proc_stat(&contents)?;
    (stat.identity().pid() == pid).then_some(stat)
}

/// Parse a `/proc/{pid}/stat` row without consulting the process table.
fn parse_proc_stat(contents: &str) -> Option<ProcStat> {
    ProcStat::parse(contents)
}

/// Read and parse `/proc/{pid}/io`.
///
/// Requires same-UID or `CAP_SYS_PTRACE`. Since brokkr spawns the child
/// process, same-UID is guaranteed.
///
/// Uses `continue` (not `?`) so one unexpected line doesn't discard the
/// entire read.
fn read_proc_io(pid: u32) -> Option<ProcIo> {
    let path = format!("/proc/{pid}/io");
    let contents = fs::read_to_string(&path).ok()?;
    parse_proc_io(&contents)
}

fn parse_proc_io(contents: &str) -> Option<ProcIo> {
    let mut io = ProcIo {
        rchar: 0,
        wchar: 0,
        read_bytes: 0,
        write_bytes: 0,
        cancelled_write_bytes: 0,
        syscr: 0,
        syscw: 0,
    };

    for line in contents.lines() {
        let (key, val) = match line.split_once(':') {
            Some(pair) => pair,
            None => continue,
        };
        let val: i64 = match val.trim().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        match key {
            "rchar" => io.rchar = val,
            "wchar" => io.wchar = val,
            "read_bytes" => io.read_bytes = val,
            "write_bytes" => io.write_bytes = val,
            "cancelled_write_bytes" => io.cancelled_write_bytes = val,
            "syscr" => io.syscr = val,
            "syscw" => io.syscw = val,
            _ => {}
        }
    }

    Some(io)
}

/// Read and parse `/proc/{pid}/status`.
fn read_proc_status(pid: u32) -> Option<ProcStatus> {
    let path = format!("/proc/{pid}/status");
    let contents = fs::read_to_string(&path).ok()?;
    parse_proc_status(&contents)
}

/// This process's peak resident set (`VmHWM`, kB) through the same status
/// parser the sampler uses. `None` when it cannot be read or is not reported.
pub(crate) fn self_peak_rss_kb() -> Option<u64> {
    let contents = fs::read_to_string("/proc/self/status").ok()?;
    let kb = parse_proc_status(&contents)?.vm_hwm_kb;
    u64::try_from(kb).ok().filter(|kb| *kb > 0)
}

fn parse_proc_status(contents: &str) -> Option<ProcStatus> {
    let mut status = ProcStatus {
        vm_rss_kb: 0,
        rss_anon_kb: 0,
        rss_file_kb: 0,
        rss_shmem_kb: 0,
        vm_swap_kb: 0,
        vm_hwm_kb: 0,
        vol_cs: 0,
        nonvol_cs: 0,
    };

    for line in contents.lines() {
        // Format: "Key:\tValue kB" or "Key:\tValue"
        let (key, rest) = match line.split_once(':') {
            Some(pair) => pair,
            None => continue,
        };
        let val_str = rest.trim().trim_end_matches(" kB");
        let val: i64 = match val_str.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        match key {
            "VmRSS" => status.vm_rss_kb = val,
            "RssAnon" => status.rss_anon_kb = val,
            "RssFile" => status.rss_file_kb = val,
            "RssShmem" => status.rss_shmem_kb = val,
            "VmSwap" => status.vm_swap_kb = val,
            "VmHWM" => status.vm_hwm_kb = val,
            "voluntary_ctxt_switches" => status.vol_cs = val,
            "nonvoluntary_ctxt_switches" => status.nonvol_cs = val,
            _ => {}
        }
    }

    Some(status)
}

/// Read all /proc metrics for a pid and assemble into a `Sample`.
///
/// All three `/proc` reads must succeed for the sample to be emitted - if
/// any fails (usually because the process exited between reads), the
/// sample is dropped entirely. Earlier versions substituted zeros for a
/// failed io read, which corrupted phase deltas (the final sample's
/// `read_bytes = 0` made `last_io.0 - first_io.0` go deeply negative on
/// the tail phase). Dropping is safer: the preceding sample's values
/// still anchor the phase correctly.
fn read_proc_metrics(pid: u32, sample_idx: i32, timestamp_us: i64) -> Option<Sample> {
    let stat = read_proc_stat(pid)?;
    let io = read_proc_io(pid)?;
    let status = read_proc_status(pid)?;

    Some(Sample {
        sample_idx,
        timestamp_us,

        rss_kb: status.vm_rss_kb,
        anon_kb: status.rss_anon_kb,
        file_kb: status.rss_file_kb,
        shmem_kb: status.rss_shmem_kb,
        swap_kb: status.vm_swap_kb,
        vm_hwm_kb: status.vm_hwm_kb,
        vsize_kb: i64::try_from(stat.virtual_bytes()).ok()? / 1024,

        utime: i64::try_from(stat.user_ticks()).ok()?,
        stime: i64::try_from(stat.system_ticks()).ok()?,
        num_threads: i64::try_from(stat.num_threads()).ok()?,

        minflt: i64::try_from(stat.minor_faults()).ok()?,
        majflt: i64::try_from(stat.major_faults()).ok()?,

        rchar: io.rchar,
        wchar: io.wchar,
        read_bytes: io.read_bytes,
        write_bytes: io.write_bytes,
        cancelled_write_bytes: io.cancelled_write_bytes,
        syscr: io.syscr,
        syscw: io.syscw,

        vol_cs: status.vol_cs,
        nonvol_cs: status.nonvol_cs,
    })
}

/// Injectable clock and metric source for sidecar sampling. The production
/// implementation reads procfs; tests can supply parsed fixtures and a
/// deterministic clock without depending on their own process row.
pub(crate) trait SidecarSampler {
    fn elapsed_since(&mut self, start: Instant) -> Duration;
    fn sample(&mut self, pid: u32, sample_idx: i32, timestamp_us: i64) -> Option<Sample>;
}

pub(crate) struct ProcSidecarSampler;

impl SidecarSampler for ProcSidecarSampler {
    fn elapsed_since(&mut self, start: Instant) -> Duration {
        start.elapsed()
    }

    fn sample(&mut self, pid: u32, sample_idx: i32, timestamp_us: i64) -> Option<Sample> {
        read_proc_metrics(pid, sample_idx, timestamp_us)
    }
}

// ---------------------------------------------------------------------------
// FIFO management
// ---------------------------------------------------------------------------

/// Where the last-marker status file lives: `~/.brokkr/sidecar-status`,
/// beside the global lock. The single definition both [`SidecarFifo`] (the
/// writer) and `brokkr lock` (the reader, via [`read_status`]) use.
///
/// Beside the lock, not under a project or scratch dir, because the reader
/// starts from the lock and nothing else: only the lock holder runs a
/// measured child, so one fixed file is unambiguous. `None` when the lock
/// directory cannot be resolved (`$HOME` unset) - the status line is a
/// convenience and is simply skipped.
///
/// Always `None` in unit tests: the file is host-global, so a test that
/// creates a [`SidecarFifo`] would otherwise delete or overwrite the status
/// of a real brokkr measuring on this machine. The pure halves
/// ([`format_status`]/[`parse_status`]) carry the tested logic.
#[cfg(not(test))]
pub(crate) fn status_path() -> Option<PathBuf> {
    crate::user_dirs::sidecar_status()
}

#[cfg(test)]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn status_path() -> Option<PathBuf> {
    None
}

/// Render the status file body: the writer's PID on the first line, the
/// marker name on the second. The PID lets the reader reject a file left
/// behind by an earlier holder that died before its `Drop` ran.
fn format_status(pid: u32, marker_name: &str) -> String {
    format!("{pid}\n{marker_name}")
}

/// Parse a status file body, returning the marker only when it was written
/// by `holder_pid`. Both PIDs come from the holder's own PID namespace (the
/// lock file records the holder's `getpid()`, and the holder is the process
/// running the sidecar), so plain equality is the right test here.
fn parse_status(body: &str, holder_pid: u32) -> Option<&str> {
    let (pid, marker) = body.split_once('\n')?;
    if pid.trim().parse::<u32>().ok()? != holder_pid {
        return None;
    }
    let marker = marker.trim();
    (!marker.is_empty()).then_some(marker)
}

/// The most recent phase marker the lock holder's sidecar has seen, if it
/// has seen one. Reader half of [`status_path`], used by `brokkr lock`.
pub(crate) fn read_status(holder_pid: u32) -> Option<String> {
    let body = fs::read_to_string(status_path()?).ok()?;
    parse_status(&body, holder_pid).map(str::to_owned)
}

/// FIFO handle: path + read end for the sidecar.
///
/// Implements `Drop` to clean up the FIFO file on panic or early error return.
pub(crate) struct SidecarFifo {
    path: PathBuf,
    reader: BufReader<File>,
    pending_line: Vec<u8>,
    pending_line_too_long: bool,
    samples_remaining: usize,
    markers_remaining: usize,
    counters_remaining: usize,
    /// Resolved once at creation; see [`status_path`].
    status: Option<PathBuf>,
}

impl SidecarFifo {
    /// Create the FIFO and open the read end.
    ///
    /// Must be called BEFORE spawning the child so the reader exists when
    /// the child tries to open the write end with `O_NONBLOCK`.
    pub(crate) fn create(
        scratch_dir: &Path,
        tunables: &MeasurementTunables,
    ) -> Result<Self, DevError> {
        let pid = std::process::id();
        let path = scratch_dir.join(format!(".sidecar-{pid}.fifo"));

        // Remove stale FIFO if it exists.
        drop(fs::remove_file(&path));

        let c_path = std::ffi::CString::new(
            path.to_str()
                .ok_or_else(|| DevError::Config("scratch path not UTF-8".into()))?,
        )
        .map_err(|e| DevError::Config(format!("invalid FIFO path: {e}")))?;

        let ret = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        if ret != 0 {
            return Err(DevError::Io(std::io::Error::last_os_error()));
        }

        let file = Self::open_read_end(&path)?;

        // A status file left by a holder that died before its Drop ran must
        // not be shown as this run's marker until this run emits one.
        let status = status_path();
        if let Some(p) = &status {
            drop(fs::remove_file(p));
        }

        Ok(Self {
            path,
            reader: BufReader::new(file),
            pending_line: Vec::with_capacity(tunables.max_fifo_line_bytes.min(256)),
            pending_line_too_long: false,
            samples_remaining: tunables.max_samples,
            markers_remaining: tunables.max_markers,
            counters_remaining: tunables.max_counters,
            status,
        })
    }

    /// Open the FIFO read end with `O_RDONLY|O_NONBLOCK`.
    fn open_read_end(path: &Path) -> Result<File, DevError> {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(DevError::Io)
    }

    /// Reopen the read end after a child exits.
    ///
    /// When the child closes the write end, the reader gets EOF. For
    /// multi-run benchmarks, we must reopen to accept the next child's
    /// write end.
    pub(crate) fn reopen(&mut self) -> Result<(), DevError> {
        let file = Self::open_read_end(&self.path)?;
        self.reader = BufReader::new(file);
        self.pending_line.clear();
        self.pending_line_too_long = false;
        Ok(())
    }

    /// Write the last marker name to the status file so `brokkr lock` can show it.
    fn update_status(&self, marker_name: &str) {
        if let Some(p) = &self.status {
            drop(fs::write(p, format_status(std::process::id(), marker_name)));
        }
    }

    /// Clean up the status file.
    fn cleanup_status(&self) {
        if let Some(p) = &self.status {
            drop(fs::remove_file(p));
        }
    }

    /// Drain all pending lines from the FIFO, parsing markers and counters.
    ///
    /// Non-blocking: returns immediately if no data is available.
    ///
    /// Protocol:
    /// - `<timestamp_us> <name>\n` - phase marker
    /// - `<timestamp_us> @<name>=<value>\n` - counter
    fn drain(
        &mut self,
        markers: &mut Vec<Marker>,
        next_marker_idx: &mut i32,
        counters: &mut Vec<Counter>,
        stop_spec: Option<&StopSpec>,
        max_line_bytes: usize,
        capture_truncated: &mut bool,
    ) -> Option<String> {
        let mut rejected_lines = 0usize;
        let mut last_name: Option<String> = None;
        let mut preferred_stop: Option<String> = None;
        let mut fallback_stop: Option<String> = None;
        loop {
            match self.read_bounded_line(max_line_bytes) {
                Ok(BoundedLine::Pending) => break,
                Ok(BoundedLine::TooLong) => {
                    rejected_lines += 1;
                    *capture_truncated = true;
                }
                Ok(BoundedLine::Line(line)) => {
                    let Ok(line) = std::str::from_utf8(&line) else {
                        rejected_lines += 1;
                        continue;
                    };
                    let trimmed = line.trim();
                    if let Some((ts_str, payload)) = trimmed.split_once(' ') {
                        let Ok(ts) = ts_str.parse::<i64>() else {
                            rejected_lines += 1;
                            continue;
                        };
                        if let Some(counter_body) = payload.strip_prefix('@') {
                            // Counter: @name=value
                            if let Some((name, val_str)) = counter_body.split_once('=')
                                && let Ok(val) = val_str.parse::<i64>()
                            {
                                if self.counters_remaining > 0 {
                                    counters.push(Counter {
                                        timestamp_us: ts,
                                        name: name.to_owned(),
                                        value: val,
                                    });
                                    self.counters_remaining -= 1;
                                } else {
                                    *capture_truncated = true;
                                }
                            } else {
                                rejected_lines += 1;
                            }
                        } else {
                            // Phase marker
                            last_name = Some(payload.to_owned());
                            if self.markers_remaining > 0 {
                                markers.push(Marker {
                                    marker_idx: *next_marker_idx,
                                    timestamp_us: ts,
                                    name: payload.to_owned(),
                                });
                                self.markers_remaining -= 1;
                            } else {
                                *capture_truncated = true;
                            }
                            *next_marker_idx = (*next_marker_idx).saturating_add(1);
                            if let Some(spec) = stop_spec {
                                match spec.match_priority(payload) {
                                    Some(true) if preferred_stop.is_none() => {
                                        preferred_stop = Some(payload.to_owned());
                                    }
                                    Some(false) if fallback_stop.is_none() => {
                                        fallback_stop = Some(payload.to_owned());
                                    }
                                    _ => {}
                                }
                            }
                        }
                    } else if !trimmed.is_empty() {
                        rejected_lines += 1;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        if rejected_lines > 0 {
            console::warn(&format!(
                "sidecar FIFO rejected {rejected_lines} malformed lines"
            ));
        }
        if let Some(name) = last_name {
            self.update_status(&name);
        }
        preferred_stop.or(fallback_stop)
    }

    /// Read one FIFO line without allowing a child to make the sidecar's
    /// temporary line buffer grow beyond `max_bytes`. An overlong record is
    /// consumed through its newline and reported as `TooLong`.
    fn read_bounded_line(&mut self, max_bytes: usize) -> std::io::Result<BoundedLine> {
        loop {
            let next = {
                let available = self.reader.fill_buf()?;
                if available.is_empty() {
                    None
                } else {
                    let line_end = available.iter().position(|byte| *byte == b'\n');
                    let consume = line_end.map_or(available.len(), |index| index + 1);
                    let body_len = line_end.unwrap_or(consume);
                    let copy_len = if self.pending_line_too_long {
                        0
                    } else {
                        body_len.min(max_bytes.saturating_sub(self.pending_line.len()))
                    };
                    Some((consume, line_end.is_some(), body_len, copy_len))
                }
            };
            let Some((consume, line_end, body_len, copy_len)) = next else {
                return Ok(self.finish_pending_line(false));
            };
            if copy_len > 0 {
                // The buffer borrow from `fill_buf` ends before `consume`.
                let available = self.reader.fill_buf()?;
                self.pending_line.extend_from_slice(&available[..copy_len]);
            }
            let line_bytes_before = self.pending_line.len().saturating_sub(copy_len);
            if !self.pending_line_too_long && body_len > max_bytes.saturating_sub(line_bytes_before)
            {
                self.pending_line_too_long = true;
                self.pending_line.clear();
            }
            self.reader.consume(consume);
            if line_end {
                return Ok(self.finish_pending_line(true));
            }
        }
    }

    fn finish_pending_line(&mut self, complete: bool) -> BoundedLine {
        if self.pending_line_too_long {
            self.pending_line_too_long = false;
            self.pending_line.clear();
            BoundedLine::TooLong
        } else if self.pending_line.is_empty() {
            if complete {
                BoundedLine::Line(Vec::new())
            } else {
                BoundedLine::Pending
            }
        } else {
            BoundedLine::Line(std::mem::take(&mut self.pending_line))
        }
    }

    fn keep_sample(&mut self) -> bool {
        if self.samples_remaining == 0 {
            return false;
        }
        self.samples_remaining -= 1;
        true
    }

    /// The FIFO path, for passing to child processes via environment variable.
    pub(crate) fn path_str(&self) -> Result<&str, DevError> {
        self.path
            .to_str()
            .ok_or_else(|| DevError::Config("FIFO path not UTF-8".into()))
    }
}

enum BoundedLine {
    Pending,
    TooLong,
    Line(Vec<u8>),
}

impl Drop for SidecarFifo {
    fn drop(&mut self) {
        drop(fs::remove_file(&self.path));
        self.cleanup_status();
    }
}

/// Get current CLOCK_MONOTONIC time in nanoseconds.
#[cfg(test)]
fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

// ---------------------------------------------------------------------------
// Sidecar loop
// ---------------------------------------------------------------------------

/// A parsed `--stop` request. Resolve the spelling once before entering the
/// sidecar loop, then compare each fresh FIFO drain against the requested name.
enum StopSpec {
    /// An explicit marker name, such as `FOO_END`.
    Exact(String),
    /// The span-close spelling `-FOO`, resolved to `FOO_END`.
    SpanEnd(String),
    /// A bare name such as `FOO`: prefer a verbatim marker, then `FOO_END`.
    Lenient { exact: String, span_end: String },
}

impl StopSpec {
    fn parse(raw: &str) -> Self {
        if let Some(suffix) = raw.strip_prefix('-') {
            return Self::SpanEnd(format!("{suffix}_END"));
        }
        if raw.ends_with("_END") {
            return Self::Exact(raw.to_owned());
        }
        Self::Lenient {
            exact: raw.to_owned(),
            span_end: format!("{raw}_END"),
        }
    }

    /// Whether a marker name matches the preferred or fallback spelling.
    /// Lenient requests prefer an exact marker over the `_END` fallback even
    /// when the fallback arrived first in the same FIFO drain.
    fn match_priority(&self, name: &str) -> Option<bool> {
        match self {
            Self::Exact(expected) | Self::SpanEnd(expected) => (name == expected).then_some(true),
            Self::Lenient { exact, .. } if name == exact => Some(true),
            Self::Lenient { span_end, .. } if name == span_end => Some(false),
            Self::Lenient { .. } => None,
        }
    }
}

/// Say why the supervisor ended the child early, when it did.
fn announce_ending(ending: &Ending) {
    if let Ending::Deadline(limit) = ending {
        console::sidecar_msg(&format!(
            "child exceeded its {:.1}s deadline, killing it",
            limit.as_secs_f64()
        ));
    } else if matches!(ending, Ending::Interrupted) {
        console::sidecar_msg("shutdown requested, killing child");
    }
}

/// The console line announcing that a stop marker fired and the child is dying.
fn stop_marker_message(requested: &str, matched: &str) -> String {
    if matched == requested {
        format!("stop marker \"{requested}\" received, killing child")
    } else {
        format!("stop marker \"{requested}\" → \"{matched}\" received, killing child")
    }
}

/// The per-run summary: `counts` is samples, markers and counters in order.
fn run_summary_line(
    run_idx: usize,
    counts: [usize; 3],
    wall_time_ms: impl std::fmt::Display,
) -> String {
    let [samples, markers, counters] = counts;
    format!(
        "sidecar run {}: {samples} samples, {markers} markers, {counters} counters, {wall_time_ms}ms",
        run_idx + 1,
    )
}

fn warn_if_truncated(truncated: bool) {
    if truncated {
        console::warn(
            "sidecar profile reached a storage limit; trajectory records or child output were omitted",
        );
    }
}

/// Run one sidecar-observed child, killing its process group once it has run
/// longer than `deadline` (measured from the caller's `start`) when one is
/// given. Process polling, stop/deadline killing,
/// output draining, and waiting are owned by the shared child supervisor;
/// this observer only samples procfs and drains the marker FIFO.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_sidecar_with_deadline(
    child_process: Child,
    fifo: &mut SidecarFifo,
    run_idx: usize,
    start: Instant,
    stop_marker: Option<&str>,
    deadline: Option<Duration>,
    tunables: &MeasurementTunables,
    sampler: &mut dyn SidecarSampler,
) -> SidecarRunResult {
    // The shared child supervisor owns process polling and process-group
    // signals. This guard only makes the sidecar's own shutdown path observe
    // the process-wide termination request.
    let shutdown_guard = crate::shutdown::SigtermGuard::install();
    let pid = child_process.id();

    let mut samples = Vec::new();
    let mut markers = Vec::new();
    let mut counters = Vec::new();
    let mut last_hwm = 0;
    let mut sample_idx = 0;
    let mut marker_idx = 0;
    let mut capture_truncated = false;
    let requested_stop = stop_marker.unwrap_or_default().to_owned();
    let stop_spec = stop_marker.map(StopSpec::parse);
    let mut fired_stop_marker = None;

    console::detail(&format!(
        "sidecar attached to pid {pid}, run {}",
        run_idx + 1
    ));

    let tick_interval = tunables.sample_interval;
    let limit = deadline.map_or(ChildLimit::None, ChildLimit::Wall);
    let label = format!("sidecar run {}", run_idx + 1);
    let options = child::ChildOptions {
        poll_interval: tick_interval,
        ..child::ChildOptions::MEASUREMENT
    };
    let run = child::supervise_child(
        child_process,
        &label,
        start,
        limit,
        crate::shutdown::ChildIsolation::ProcessGroup(&shutdown_guard),
        options,
        || {
            let elapsed_us =
                i64::try_from(sampler.elapsed_since(start).as_micros()).unwrap_or(i64::MAX);
            if let Some(sample) = sampler.sample(pid, sample_idx, elapsed_us) {
                sample_idx = sample_idx.saturating_add(1);
                last_hwm = last_hwm.max(sample.vm_hwm_kb);
                if fifo.keep_sample() {
                    samples.push(sample);
                } else {
                    capture_truncated = true;
                }
            }

            let matched = fifo.drain(
                &mut markers,
                &mut marker_idx,
                &mut counters,
                stop_spec.as_ref(),
                tunables.max_fifo_line_bytes,
                &mut capture_truncated,
            );
            let matched = matched?;
            fired_stop_marker = Some(matched.clone());
            console::sidecar_msg(&stop_marker_message(&requested_stop, &matched));
            Some(matched)
        },
    );

    fifo.drain(
        &mut markers,
        &mut marker_idx,
        &mut counters,
        None,
        tunables.max_fifo_line_bytes,
        &mut capture_truncated,
    );
    let (stdout_truncated, stderr_truncated) = (run.stdout_truncated, run.stderr_truncated);
    capture_truncated |= stdout_truncated || stderr_truncated || run.output_cut;
    warn_if_truncated(capture_truncated);
    announce_ending(&run.ending);

    // The shared nearest-millisecond rule, so the sidecar summary agrees with
    // the results row's rounded wall.
    let wall_time_ms = crate::harness::elapsed_to_ms(&run.elapsed);
    console::detail(&run_summary_line(
        run_idx,
        [samples.len(), markers.len(), counters.len()],
        wall_time_ms,
    ));

    let sample_count = i32::try_from(samples.len()).unwrap_or(i32::MAX);
    let marker_count = i32::try_from(markers.len()).unwrap_or(i32::MAX);

    SidecarRunResult {
        stdout: run.stdout,
        stderr: run.stderr,
        stdout_truncated,
        stderr_truncated,
        output_cut: run.output_cut,
        elapsed: run.elapsed,
        data: SidecarData {
            samples,
            markers,
            counters,
            fired_stop_marker,
            ending: run.ending,
            summary: SidecarSummary {
                vm_hwm_kb: last_hwm,
                sample_count,
                marker_count,
                wall_time_ms,
                capture_truncated,
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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

    #[test]
    fn parse_proc_stat_self() {
        let pid = std::process::id();
        let stat = read_proc_stat(pid);
        assert!(stat.is_some(), "should be able to read own /proc/stat");
        let stat = stat.unwrap();
        assert!(stat.num_threads() >= 1);
        assert!(stat.virtual_bytes() > 0);
    }

    #[test]
    fn parse_proc_status_self() {
        let pid = std::process::id();
        let status = read_proc_status(pid);
        assert!(status.is_some(), "should be able to read own /proc/status");
        let status = status.unwrap();
        assert!(status.vm_rss_kb > 0);
        assert!(status.vm_hwm_kb > 0);
    }

    #[test]
    fn parse_proc_io_self() {
        let pid = std::process::id();
        let io = read_proc_io(pid);
        assert!(io.is_some(), "should be able to read own /proc/io");
        let io = io.unwrap();
        assert!(io.rchar > 0);
    }

    #[test]
    fn full_sample_self() {
        let pid = std::process::id();
        let sample = read_proc_metrics(pid, 0, 12345);
        assert!(sample.is_some());
        let sample = sample.unwrap();
        assert_eq!(sample.sample_idx, 0);
        assert_eq!(sample.timestamp_us, 12345);
        assert!(sample.rss_kb > 0);
        assert!(sample.utime >= 0);
        assert!(sample.num_threads >= 1);
    }

    #[test]
    fn nonexistent_pid_returns_none() {
        let stat = read_proc_stat(0);
        assert!(stat.is_none());
    }

    #[test]
    fn monotonic_clock_advances() {
        let t1 = monotonic_ns();
        let t2 = monotonic_ns();
        assert!(t2 >= t1);
    }

    #[test]
    fn status_round_trips_for_its_writer() {
        let body = format_status(4242, "STAGE2_START");
        assert_eq!(parse_status(&body, 4242), Some("STAGE2_START"));
    }

    /// A file left by a holder that died before its Drop ran names a PID
    /// other than the current holder's, and must not be shown.
    #[test]
    fn status_from_another_pid_is_rejected() {
        let body = format_status(4242, "STAGE2_START");
        assert_eq!(parse_status(&body, 7), None);
    }

    #[test]
    fn status_rejects_legacy_and_empty_bodies() {
        // The pre-fix format was the bare marker name, no PID line.
        assert_eq!(parse_status("STAGE2_START", 4242), None);
        assert_eq!(parse_status("4242\n", 4242), None);
        assert_eq!(parse_status("x\nSTAGE", 4242), None);
    }

    #[test]
    fn read_proc_io_tolerates_extra_lines() {
        let io = parse_proc_io(concat!(
            "rchar: 12\n",
            "wchar: 34\n",
            "unexpected line\n",
            "read_bytes: 56\n",
            "write_bytes: invalid\n",
            "cancelled_write_bytes: 7\n",
            "syscr: 8\n",
            "syscw: 9\n",
            "future_counter: 100\n",
        ))
        .expect("unexpected lines are ignored");
        assert_eq!(io.rchar, 12);
        assert_eq!(io.wchar, 34);
        assert_eq!(io.read_bytes, 56);
        assert_eq!(io.write_bytes, 0, "invalid values leave the default");
        assert_eq!(io.cancelled_write_bytes, 7);
        assert_eq!(io.syscr, 8);
        assert_eq!(io.syscw, 9);
    }
}
