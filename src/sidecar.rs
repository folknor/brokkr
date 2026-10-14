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
//! run longer than the given limit; [`run_sidecar`] takes its limit from the
//! enclosing [`DeadlineScope`], if any. The scope exists so a caller several
//! layers above the sidecar (a measured command driving
//! `harness::run_hotpath_capture`) can bound a hung child without threading a
//! parameter through every capture helper.
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

/// Sample interval in microseconds (100ms).
const SAMPLE_INTERVAL_US: u64 = 100_000;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single point-in-time sample of process metrics from `/proc`.
#[derive(Debug, Clone)]
pub struct Sample {
    pub sample_idx: i32,
    pub timestamp_us: i64,

    // Process memory (from /proc/{pid}/status, in kB)
    pub rss_kb: i64,
    pub anon_kb: i64,
    pub file_kb: i64,
    pub shmem_kb: i64,
    pub swap_kb: i64,
    pub vm_hwm_kb: i64,

    // Virtual memory size (from /proc/{pid}/stat, converted to kB)
    pub vsize_kb: i64,

    // CPU (raw clock ticks from /proc/{pid}/stat)
    pub utime: i64,
    pub stime: i64,
    pub num_threads: i64,

    // Page faults (cumulative, from /proc/{pid}/stat)
    pub minflt: i64,
    pub majflt: i64,

    // I/O (cumulative bytes, from /proc/{pid}/io)
    pub rchar: i64,
    pub wchar: i64,
    pub read_bytes: i64,
    pub write_bytes: i64,
    pub cancelled_write_bytes: i64,
    pub syscr: i64,
    pub syscw: i64,

    // Context switches (cumulative, from /proc/{pid}/status)
    pub vol_cs: i64,
    pub nonvol_cs: i64,
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
/// `_START`/`_END` spans, phase-name lookup, and stop-marker resolution.
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

    /// Build a timeline over a fresh marker batch while a child is running.
    fn marker_batch(markers: &'a [Marker]) -> Self {
        Self {
            markers,
            final_us: 0,
            first_sample_us: None,
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

    /// Resolve the parsed stop spelling against this marker batch.
    fn stop_marker(&self, spec: &StopSpec) -> Option<&'a Marker> {
        let first_named = |name: &str| self.markers.iter().find(|marker| marker.name == name);
        match spec {
            StopSpec::Exact(name) | StopSpec::SpanEnd(name) => first_named(name),
            StopSpec::Lenient { exact, span_end } => {
                first_named(exact).or_else(|| first_named(span_end))
            }
        }
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
/// Contains captured output, the child's ending and wall-clock time, and the
/// sidecar profile data.
pub struct SidecarRunResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
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
    ProcStat::read(Pid::new(pid)?).ok()
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
    /// Resolved once at creation; see [`status_path`].
    status: Option<PathBuf>,
}

impl SidecarFifo {
    /// Create the FIFO and open the read end.
    ///
    /// Must be called BEFORE spawning the child so the reader exists when
    /// the child tries to open the write end with `O_NONBLOCK`.
    pub(crate) fn create(scratch_dir: &Path) -> Result<Self, DevError> {
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
    ) {
        let mut last_name: Option<String> = None;
        let mut line = String::new();
        loop {
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = line.trim();
                    if let Some((ts_str, payload)) = trimmed.split_once(' ') {
                        let Ok(ts) = ts_str.parse::<i64>() else {
                            continue;
                        };
                        if let Some(counter_body) = payload.strip_prefix('@') {
                            // Counter: @name=value
                            if let Some((name, val_str)) = counter_body.split_once('=')
                                && let Ok(val) = val_str.parse::<i64>()
                            {
                                counters.push(Counter {
                                    timestamp_us: ts,
                                    name: name.to_owned(),
                                    value: val,
                                });
                            }
                        } else {
                            // Phase marker
                            last_name = Some(payload.to_owned());
                            markers.push(Marker {
                                marker_idx: *next_marker_idx,
                                timestamp_us: ts,
                                name: payload.to_owned(),
                            });
                            *next_marker_idx += 1;
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        if let Some(name) = last_name {
            self.update_status(&name);
        }
    }

    /// The FIFO path, for passing to child processes via environment variable.
    pub(crate) fn path_str(&self) -> Result<&str, DevError> {
        self.path
            .to_str()
            .ok_or_else(|| DevError::Config("FIFO path not UTF-8".into()))
    }
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
/// sidecar loop, then compare each fresh marker batch against the stored names.
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

    /// Return the actual marker name that matched in this fresh batch.
    fn matching_marker(&self, timeline: &Timeline<'_>) -> Option<String> {
        timeline.stop_marker(self).map(|marker| marker.name.clone())
    }
}

thread_local! {
    /// The deadline [`run_sidecar`] applies, set by the innermost live
    /// [`DeadlineScope`] on this thread. Thread-local because the sidecar
    /// loop runs on the thread that entered the scope.
    static AMBIENT_DEADLINE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Bounds every [`run_sidecar`] call on this thread while it lives: a child
/// running longer than `limit` (measured from the caller's `start`) has its
/// process group SIGKILLed and the run records `Ending::Deadline`.
///
/// Scopes nest; dropping one restores the enclosing limit.
pub(crate) struct DeadlineScope {
    prev: Option<Duration>,
}

impl DeadlineScope {
    pub(crate) fn enter(limit: Duration) -> Self {
        let prev = AMBIENT_DEADLINE.with(|c| c.replace(Some(limit)));
        Self { prev }
    }
}

impl Drop for DeadlineScope {
    fn drop(&mut self) {
        AMBIENT_DEADLINE.with(|c| c.set(self.prev));
    }
}

fn ambient_deadline() -> Option<Duration> {
    AMBIENT_DEADLINE.with(std::cell::Cell::get)
}

/// Run the sidecar sampling loop for a single benchmark run, bounded by the
/// enclosing [`DeadlineScope`] if there is one (unbounded otherwise).
///
/// See [`run_sidecar_with_deadline`].
pub(crate) fn run_sidecar(
    child_process: Child,
    fifo: &mut SidecarFifo,
    run_idx: usize,
    start: Instant,
    stop_marker: Option<&str>,
) -> SidecarRunResult {
    run_sidecar_with_deadline(
        child_process,
        fifo,
        run_idx,
        start,
        stop_marker,
        ambient_deadline(),
    )
}

/// Run one sidecar-observed child. Process polling, stop/deadline killing,
/// output draining, and waiting are owned by the shared child supervisor;
/// this observer only samples procfs and drains the marker FIFO.
pub(crate) fn run_sidecar_with_deadline(
    child_process: Child,
    fifo: &mut SidecarFifo,
    run_idx: usize,
    start: Instant,
    stop_marker: Option<&str>,
    deadline: Option<Duration>,
) -> SidecarRunResult {
    let _shutdown_guard = crate::shutdown::SigtermGuard::install();
    let pid = child_process.id();

    let mut samples = Vec::new();
    let mut markers = Vec::new();
    let mut counters = Vec::new();
    let mut last_hwm = 0;
    let mut sample_idx = 0;
    let mut marker_idx = 0;
    let requested_stop = stop_marker.unwrap_or_default().to_owned();
    let stop_spec = stop_marker.map(StopSpec::parse);
    let mut fired_stop_marker = None;

    console::detail(&format!(
        "sidecar attached to pid {pid}, run {}",
        run_idx + 1
    ));

    let tick_interval = Duration::from_micros(SAMPLE_INTERVAL_US);
    let limit = deadline.map_or(ChildLimit::None, ChildLimit::Wall);
    let label = format!("sidecar run {}", run_idx + 1);
    let run = child::supervise_child(
        child_process,
        &label,
        start,
        limit,
        true,
        tick_interval,
        || {
            let elapsed_us = i64::try_from(start.elapsed().as_micros()).unwrap_or(i64::MAX);
            if let Some(sample) = read_proc_metrics(pid, sample_idx, elapsed_us) {
                last_hwm = last_hwm.max(sample.vm_hwm_kb);
                samples.push(sample);
                sample_idx += 1;
            }

            let marker_count_before = markers.len();
            fifo.drain(&mut markers, &mut marker_idx, &mut counters);
            let Some(spec) = &stop_spec else {
                return None;
            };
            let timeline = Timeline::marker_batch(&markers[marker_count_before..]);
            let matched = spec.matching_marker(&timeline)?;
            fired_stop_marker = Some(matched.clone());
            let display = if matched == requested_stop {
                format!("stop marker \"{requested_stop}\" received, killing child")
            } else {
                format!("stop marker \"{requested_stop}\" → \"{matched}\" received, killing child")
            };
            console::sidecar_msg(&display);
            Some(matched)
        },
    );

    fifo.drain(&mut markers, &mut marker_idx, &mut counters);
    if let Ending::Deadline(limit) = &run.ending {
        console::sidecar_msg(&format!(
            "child exceeded its {:.1}s deadline, killing it",
            limit.as_secs_f64()
        ));
    } else if matches!(&run.ending, Ending::Interrupted) {
        console::sidecar_msg("shutdown requested, killing child");
    }

    let wall_time_ms = i64::try_from(run.elapsed.as_millis()).unwrap_or(i64::MAX);
    console::detail(&format!(
        "sidecar run {}: {} samples, {} markers, {} counters, {wall_time_ms}ms",
        run_idx + 1,
        samples.len(),
        markers.len(),
        counters.len(),
    ));

    SidecarRunResult {
        stdout: run.stdout,
        stderr: run.stderr,
        elapsed: run.elapsed,
        data: SidecarData {
            samples,
            markers,
            counters,
            fired_stop_marker,
            ending: run.ending,
            summary: SidecarSummary {
                vm_hwm_kb: last_hwm,
                sample_count: sample_idx,
                marker_count: marker_idx,
                wall_time_ms,
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
    fn deadline_scopes_nest_and_restore() {
        assert_eq!(ambient_deadline(), None);
        {
            let _outer = DeadlineScope::enter(Duration::from_secs(60));
            assert_eq!(ambient_deadline(), Some(Duration::from_secs(60)));
            {
                let _inner = DeadlineScope::enter(Duration::from_secs(5));
                assert_eq!(ambient_deadline(), Some(Duration::from_secs(5)));
            }
            assert_eq!(ambient_deadline(), Some(Duration::from_secs(60)));
        }
        assert_eq!(ambient_deadline(), None);
    }

    #[test]
    fn read_proc_io_tolerates_extra_lines() {
        // Simulate a /proc/io with an unexpected line - should not crash.
        // We can't easily inject content into /proc, but we verify our own
        // /proc/io parses correctly (which has the standard 7 lines).
        let pid = std::process::id();
        let io = read_proc_io(pid);
        assert!(io.is_some());
        let io = io.unwrap();
        // All standard fields should be populated.
        assert!(
            io.syscr > 0 || io.rchar > 0,
            "at least one I/O counter should be nonzero"
        );
    }
}
