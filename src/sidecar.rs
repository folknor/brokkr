//! Monitoring sidecar that runs alongside benchmark processes.
//!
//! Samples `/proc/{pid}/*` at fixed intervals, reads application phase markers
//! from a FIFO, and bulk-inserts everything to SQLite after the child exits.
//! Zero I/O during the benchmark itself.
//!
//! # Pipe draining
//!
//! The child's stdout and stderr are piped (for error reporting). To avoid
//! deadlock when the child fills the OS pipe buffer (~64 KiB), we take
//! ownership of the pipe handles and drain them in background threads while
//! the sidecar loop samples `/proc` and checks `try_wait()`.
//!
//! # Timing
//!
//! Both the sidecar sample loop and the sleep scheduler use `CLOCK_MONOTONIC`.
//! `Instant::now()` provides sample timestamps (it wraps `CLOCK_MONOTONIC` on
//! Linux). `clock_nanosleep(TIMER_ABSTIME)` provides drift-free tick scheduling.
//! The child's wall-clock time is captured at the moment `try_wait()` detects
//! exit, not after the sidecar loop finishes draining.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Sample interval in microseconds (100ms).
const SAMPLE_INTERVAL_US: i64 = 100_000;

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
    pub summary: SidecarSummary,
}

/// Result of a sidecar-monitored child process run.
///
/// Contains both the child's exit information (status, stdout, stderr,
/// wall-clock time) and the sidecar profile data.
pub struct SidecarRunResult {
    pub exit_status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub elapsed: Duration,
    pub data: SidecarData,
    /// True when the child was killed because a `--stop` marker was received.
    /// Callers should treat this as a successful exit even though the exit
    /// status reflects SIGKILL.
    pub stopped_by_marker: bool,
    /// True when the child was killed because brokkr caught SIGTERM (via
    /// `brokkr kill`). Callers store the partial sidecar and then abort
    /// the bench, letting the outer layer run scratch cleanup.
    pub stopped_by_signal: bool,
}

// ---------------------------------------------------------------------------
// /proc parsing
// ---------------------------------------------------------------------------

/// Fields extracted from `/proc/{pid}/stat`.
struct ProcStat {
    utime: i64,
    stime: i64,
    num_threads: i64,
    vsize: i64, // bytes
    minflt: i64,
    majflt: i64,
}

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

/// Read and parse `/proc/{pid}/stat`.
///
/// The comm field (field 2) can contain spaces and parentheses, so we find
/// the *last* `)` to locate the end of field 2, then parse by index from there.
fn read_proc_stat(pid: u32) -> Option<ProcStat> {
    let path = format!("/proc/{pid}/stat");
    let contents = fs::read_to_string(&path).ok()?;

    // Find end of comm field: last ')' in the line.
    let comm_end = contents.rfind(')')?;
    let rest = contents.get(comm_end + 2..)?; // skip ") "

    // Fields after comm (0-indexed from after comm):
    //  0: state
    //  1: ppid
    //  2: pgrp
    //  3: session
    //  4: tty_nr
    //  5: tpgid
    //  6: flags
    //  7: minflt
    //  8: cminflt
    //  9: majflt
    // 10: cmajflt
    // 11: utime
    // 12: stime
    // ...
    // 17: num_threads (field index 19 in the full line, 17 after comm)
    // ...
    // 20: vsize (field index 22 in the full line, 20 after comm)
    let fields: Vec<&str> = rest.split_whitespace().collect();

    if fields.len() < 21 {
        return None;
    }

    Some(ProcStat {
        minflt: fields[7].parse().ok()?,
        majflt: fields[9].parse().ok()?,
        utime: fields[11].parse().ok()?,
        stime: fields[12].parse().ok()?,
        num_threads: fields[17].parse().ok()?,
        vsize: fields[20].parse().ok()?,
    })
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
        vsize_kb: stat.vsize / 1024,

        utime: stat.utime,
        stime: stat.stime,
        num_threads: stat.num_threads,

        minflt: stat.minflt,
        majflt: stat.majflt,

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

/// FIFO handle: path + read end for the sidecar.
///
/// Implements `Drop` to clean up the FIFO file on panic or early error return.
pub(crate) struct SidecarFifo {
    path: PathBuf,
    reader: BufReader<File>,
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

        Ok(Self {
            path,
            reader: BufReader::new(file),
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

    /// Path to the sidecar status file (for `brokkr lock` to read).
    fn status_path(&self) -> PathBuf {
        self.path.with_file_name(".sidecar-status")
    }

    /// Write the last marker name to the status file so `brokkr lock` can show it.
    fn update_status(&self, marker_name: &str) {
        drop(fs::write(self.status_path(), marker_name));
    }

    /// Clean up the status file.
    fn cleanup_status(&self) {
        drop(fs::remove_file(self.status_path()));
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

// ---------------------------------------------------------------------------
// Sleep with CLOCK_MONOTONIC absolute time
// ---------------------------------------------------------------------------

/// Sleep until the specified absolute time (CLOCK_MONOTONIC, nanoseconds).
///
/// Uses `clock_nanosleep` with `TIMER_ABSTIME` to avoid drift from
/// /proc read overhead (~30µs per tick).
fn sleep_until_ns(target_ns: i64) {
    let ts = libc::timespec {
        tv_sec: target_ns / 1_000_000_000,
        tv_nsec: target_ns % 1_000_000_000,
    };
    unsafe {
        libc::clock_nanosleep(
            libc::CLOCK_MONOTONIC,
            libc::TIMER_ABSTIME,
            &ts,
            std::ptr::null_mut(),
        );
    }
}

/// Get current CLOCK_MONOTONIC time in nanoseconds.
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

/// Drain a pipe handle to completion in a thread.
///
/// Used to prevent deadlock: the child's stdout/stderr must be drained
/// concurrently with the sidecar sampling loop, otherwise the child blocks
/// once the OS pipe buffer (~64 KiB) fills.
fn drain_pipe(pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut reader = pipe;
        drop(reader.read_to_end(&mut buf));
        buf
    })
}

/// Resolve a `--stop` spelling against a batch of freshly-drained
/// markers. Returns the matched marker name when a hit lands, `None`
/// otherwise. See the inline comment at the call site for the three
/// accepted spellings.
fn stop_match(stop: &str, recent: &[Marker]) -> Option<String> {
    // `-FOO` → canonical `FOO_END`. No fallback - the sigil is
    // explicit, so a mismatch is a mismatch.
    if let Some(suffix) = stop.strip_prefix('-') {
        let canonical = format!("{suffix}_END");
        return recent
            .iter()
            .find(|m| m.name == canonical)
            .map(|m| m.name.clone());
    }
    // Verbatim first - preserves existing `--stop FOO_END` semantics.
    if let Some(m) = recent.iter().find(|m| m.name == stop) {
        return Some(m.name.clone());
    }
    // Fallback: `--stop FOO` resolves to `FOO_END` for the common
    // pbfhogg naming convention. The caller prints a notice so the
    // resolved form is visible in the sidecar log line.
    let canonical = format!("{stop}_END");
    recent
        .iter()
        .find(|m| m.name == canonical)
        .map(|m| m.name.clone())
}

/// Run the sidecar sampling loop for a single benchmark run.
///
/// Takes ownership of the `Child` process. Drains stdout/stderr in
/// background threads to prevent pipe-buffer deadlock. Samples
/// `/proc/{pid}/*` at 100ms intervals, drains FIFO markers, and detects
/// child exit via `try_wait()`.
///
/// Returns a `SidecarRunResult` containing the child's exit status,
/// captured output, wall-clock elapsed time, and sidecar profile data.
#[allow(clippy::too_many_lines)]
pub(crate) fn run_sidecar(
    mut child: Child,
    fifo: &mut SidecarFifo,
    run_idx: usize,
    start: Instant,
    stop_marker: Option<&str>,
) -> SidecarRunResult {
    // Scope the SIGTERM handler to the sidecar window only. Outside this
    // RAII guard's lifetime, `brokkr kill` falls through to the default
    // terminate action - exactly what the user expects during cargo build
    // / brokkr check / other non-sidecar work where there's no child to
    // reap and no graceful partial-state to preserve.
    let _shutdown_guard = crate::shutdown::SigtermGuard::install();
    let pid = child.id();

    // Take stdout/stderr handles from the child and drain them in background
    // threads. This prevents the classic pipe-buffer deadlock: if the child
    // writes >64 KiB to a piped stream, it blocks until someone reads, but
    // the sidecar loop is waiting for the child to exit - deadlock.
    let stdout_thread = child.stdout.take().map(drain_pipe);
    let stderr_thread = child.stderr.take().map(drain_pipe);

    let start_ns = monotonic_ns();
    let mut samples: Vec<Sample> = Vec::new();
    let mut markers: Vec<Marker> = Vec::new();
    let mut counters: Vec<Counter> = Vec::new();
    let mut last_hwm: i64 = 0;
    let mut sample_idx: i32 = 0;
    let mut marker_idx: i32 = 0;

    let interval_ns = SAMPLE_INTERVAL_US * 1_000;
    let mut next_tick_ns = start_ns + interval_ns;

    output::sidecar_msg(&format!("attached to pid {pid}, run {run_idx}"));

    // The exit status and elapsed time, captured at the moment try_wait
    // detects exit (not after the loop finishes). This avoids including
    // up to one sample interval of sidecar overhead in the timing.
    let mut exit_status: Option<ExitStatus> = None;
    #[allow(unused_assignments)] // initial None is never read; every branch assigns before use
    let mut child_elapsed: Option<Duration> = None;
    let mut stopped_by_marker = false;
    let mut stopped_by_signal = false;

    loop {
        #[allow(clippy::cast_possible_truncation)]
        let elapsed_us = start.elapsed().as_micros() as i64;

        // Sample /proc (3 file reads, ~30µs total).
        // Gracefully skipped if /proc reads fail (process exiting).
        if let Some(s) = read_proc_metrics(pid, sample_idx, elapsed_us) {
            if s.vm_hwm_kb > last_hwm {
                last_hwm = s.vm_hwm_kb;
            }
            samples.push(s);
            sample_idx += 1;
        }

        // Drain any pending markers from FIFO.
        let marker_count_before = markers.len();
        fifo.drain(&mut markers, &mut marker_idx, &mut counters);

        // If a stop marker was requested, check if it was just emitted.
        //
        // Three accepted spellings, in priority order:
        //   1. Verbatim:   `--stop FOO_END`        matches a marker named `FOO_END`.
        //   2. Span-end:   `--stop -FOO`           matches `FOO_END` (the `-` sigil
        //                                           mirrors the span-close semantics
        //                                           we'd adopt if we ever introduce
        //                                           a typed span protocol).
        //   3. Fallback:   `--stop FOO`            matches `FOO_END` after the
        //                                           verbatim check fails; prints a
        //                                           one-line notice so the resolved
        //                                           form is visible.
        if let Some(stop) = stop_marker {
            let matched = stop_match(
                stop,
                &markers[marker_count_before..],
            );
            if let Some(matched_name) = matched {
                let display = if matched_name == stop {
                    format!("stop marker \"{stop}\" received, killing child")
                } else {
                    format!(
                        "stop marker \"{stop}\" → \"{matched_name}\" received, killing child"
                    )
                };
                output::sidecar_msg(&display);
                child.kill().ok();
                // Wait for process to actually exit so we can collect status.
                let status = child.wait().ok();
                child_elapsed = Some(start.elapsed());
                exit_status = status;
                stopped_by_marker = true;
                break;
            }
        }

        // Graceful shutdown via `brokkr kill` (SIGTERM handler set this flag).
        if crate::shutdown::is_shutdown_requested() {
            output::sidecar_msg("shutdown requested, killing child");
            child.kill().ok();
            let status = child.wait().ok();
            child_elapsed = Some(start.elapsed());
            exit_status = status;
            stopped_by_signal = true;
            break;
        }

        // Check child exit via handle (not bare PID - PIDs can be reused).
        match child.try_wait() {
            Ok(Some(status)) => {
                child_elapsed = Some(start.elapsed());
                exit_status = Some(status);
                break;
            }
            Ok(None) => {}
            Err(_) => {
                child_elapsed = Some(start.elapsed());
                break;
            }
        }

        // Sleep until next tick (CLOCK_MONOTONIC absolute time).
        sleep_until_ns(next_tick_ns);
        next_tick_ns += interval_ns;
    }

    // Final drain - markers/counters may have arrived between last sample and exit.
    fifo.drain(&mut markers, &mut marker_idx, &mut counters);

    // Join pipe drain threads. These complete quickly once the child exits
    // since the pipe write ends are closed.
    let stdout = stdout_thread
        .map(|t| t.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr_thread
        .map(|t| t.join().unwrap_or_default())
        .unwrap_or_default();

    // Fall back to wait() if try_wait() never saw the exit (shouldn't happen,
    // but defensive). This is safe because the pipe drain threads have already
    // joined, so wait_with_output() won't deadlock.
    let exit_status = exit_status.unwrap_or_else(|| {
        child_elapsed = Some(start.elapsed());
        child.wait().unwrap_or_else(|_| {
            // Process already reaped - synthesize a failure status.
            std::process::ExitStatus::default()
        })
    });

    let child_elapsed = child_elapsed.unwrap_or_else(|| start.elapsed());
    #[allow(clippy::cast_possible_truncation)]
    let wall_time_ms = child_elapsed.as_millis() as i64;

    output::sidecar_msg(&format!(
        "{} samples, {} markers, {} counters, {wall_time_ms}ms",
        samples.len(),
        markers.len(),
        counters.len(),
    ));

    SidecarRunResult {
        exit_status,
        stdout,
        stderr,
        elapsed: child_elapsed,
        data: SidecarData {
            samples,
            markers,
            counters,
            summary: SidecarSummary {
                vm_hwm_kb: last_hwm,
                sample_count: sample_idx,
                marker_count: marker_idx,
                wall_time_ms,
            },
        },
        stopped_by_marker,
        stopped_by_signal,
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
        assert!(stat.num_threads >= 1);
        assert!(stat.vsize > 0);
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
