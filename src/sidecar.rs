//! Monitoring sidecar that runs alongside benchmark processes.
//!
//! Samples `/proc/{pid}/*` at fixed intervals, reads application phase markers
//! from a FIFO, and bulk-inserts everything to SQLite after the child exits.
//! Zero I/O during the benchmark itself.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Instant;

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
    pub summary: SidecarSummary,
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
    // 21: rss   (field index 23, 21 after comm)
    let fields: Vec<&str> = rest.split_whitespace().collect();

    if fields.len() < 22 {
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
        let (key, val) = line.split_once(':')?;
        let val: i64 = val.trim().parse().ok()?;
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
fn read_proc_metrics(pid: u32, sample_idx: i32, timestamp_us: i64) -> Option<Sample> {
    let stat = read_proc_stat(pid)?;
    let io = read_proc_io(pid);
    let status = read_proc_status(pid)?;

    let io = io.unwrap_or(ProcIo {
        rchar: 0,
        wchar: 0,
        read_bytes: 0,
        write_bytes: 0,
        cancelled_write_bytes: 0,
        syscr: 0,
        syscw: 0,
    });

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
pub struct SidecarFifo {
    pub path: PathBuf,
    reader: BufReader<File>,
}

impl SidecarFifo {
    /// Create the FIFO and open the read end.
    ///
    /// Must be called BEFORE spawning the child so the reader exists when
    /// the child tries to open the write end with `O_NONBLOCK`.
    pub fn create(scratch_dir: &Path) -> Result<Self, DevError> {
        let pid = std::process::id();
        let path = scratch_dir.join(format!(".sidecar-{pid}.fifo"));

        // Remove stale FIFO if it exists.
        let _ = fs::remove_file(&path);

        let c_path = std::ffi::CString::new(
            path.to_str()
                .ok_or_else(|| DevError::Config("scratch path not UTF-8".into()))?,
        )
        .map_err(|e| DevError::Config(format!("invalid FIFO path: {e}")))?;

        let ret = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        if ret != 0 {
            return Err(DevError::Io(std::io::Error::last_os_error()));
        }

        // Open read end. O_RDONLY|O_NONBLOCK so we don't block waiting for a writer.
        // We'll use non-blocking reads in the sidecar loop.
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)?;

        Ok(Self {
            path,
            reader: BufReader::new(file),
        })
    }

    /// Drain all pending marker lines from the FIFO.
    ///
    /// Non-blocking: returns immediately if no data is available.
    /// Each line is expected to be: `<timestamp_us> <marker_name>\n`
    fn drain_markers(&mut self, markers: &mut Vec<Marker>, next_idx: &mut i32) {
        let mut line = String::new();
        loop {
            line.clear();
            match self.reader.read_line(&mut line) {
                Ok(0) => break, // EOF or no data
                Ok(_) => {
                    let trimmed = line.trim();
                    if let Some((ts_str, name)) = trimmed.split_once(' ') {
                        if let Ok(ts) = ts_str.parse::<i64>() {
                            markers.push(Marker {
                                marker_idx: *next_idx,
                                timestamp_us: ts,
                                name: name.to_owned(),
                            });
                            *next_idx += 1;
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    /// Clean up the FIFO file.
    pub fn cleanup(self) {
        let _ = fs::remove_file(&self.path);
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
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

// ---------------------------------------------------------------------------
// Sidecar loop
// ---------------------------------------------------------------------------

/// Run the sidecar sampling loop for a single benchmark run.
///
/// Samples `/proc/{pid}/*` at 100ms intervals, drains FIFO markers, and
/// returns all collected data when the child exits.
///
/// The caller must pass the `Child` handle so we detect exit via
/// `try_wait()`, not bare PID liveness checks.
pub fn run_sidecar(
    child: &mut Child,
    fifo: &mut SidecarFifo,
    run_idx: usize,
) -> SidecarData {
    let pid = child.id();
    let start = Instant::now();
    let start_ns = monotonic_ns();

    let mut samples: Vec<Sample> = Vec::new();
    let mut markers: Vec<Marker> = Vec::new();
    let mut last_hwm: i64 = 0;
    let mut sample_idx: i32 = 0;
    let mut marker_idx: i32 = 0;

    let interval_ns = SAMPLE_INTERVAL_US * 1_000;
    let mut next_tick_ns = start_ns + interval_ns;

    output::sidecar_msg(&format!("attached to pid {pid}, run {run_idx}"));

    loop {
        let elapsed_us = start.elapsed().as_micros() as i64;

        // Sample /proc (3 file reads, ~30µs total).
        if let Some(s) = read_proc_metrics(pid, sample_idx, elapsed_us) {
            if s.vm_hwm_kb > last_hwm {
                last_hwm = s.vm_hwm_kb;
            }
            samples.push(s);
            sample_idx += 1;
        }

        // Drain any pending markers from FIFO.
        fifo.drain_markers(&mut markers, &mut marker_idx);

        // Check child exit via handle (not bare PID).
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => break,
        }

        // Sleep until next tick (CLOCK_MONOTONIC absolute time).
        sleep_until_ns(next_tick_ns);
        next_tick_ns += interval_ns;
    }

    // Final drain — markers may have arrived between last sample and exit.
    fifo.drain_markers(&mut markers, &mut marker_idx);

    let wall_time_ms = start.elapsed().as_millis() as i64;

    output::sidecar_msg(&format!(
        "{} samples, {} markers, {wall_time_ms}ms",
        samples.len(),
        markers.len(),
    ));

    SidecarData {
        samples,
        markers,
        summary: SidecarSummary {
            vm_hwm_kb: last_hwm,
            sample_count: sample_idx,
            marker_count: marker_idx,
            wall_time_ms,
        },
    }
}

// ---------------------------------------------------------------------------
// SQLite storage
// ---------------------------------------------------------------------------

/// Bulk-insert sidecar data for one run into the results database.
pub fn store_sidecar_data(
    conn: &rusqlite::Connection,
    result_uuid: &str,
    run_idx: usize,
    data: &SidecarData,
) -> Result<(), DevError> {
    let tx = conn.unchecked_transaction()?;

    // Insert samples.
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO sidecar_samples (
                result_uuid, run_idx, sample_idx, timestamp_us,
                rss_kb, anon_kb, file_kb, shmem_kb, swap_kb, vsize_kb, vm_hwm_kb,
                utime, stime, num_threads, minflt, majflt,
                rchar, wchar, read_bytes, write_bytes, cancelled_write_bytes,
                syscr, syscw, vol_cs, nonvol_cs
            ) VALUES (
                ?1, ?2, ?3, ?4,
                ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                ?12, ?13, ?14, ?15, ?16,
                ?17, ?18, ?19, ?20, ?21,
                ?22, ?23, ?24, ?25
            )",
        )?;

        for s in &data.samples {
            stmt.execute(rusqlite::params![
                result_uuid,
                run_idx as i64,
                s.sample_idx,
                s.timestamp_us,
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
            ])?;
        }
    }

    // Insert markers.
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO sidecar_markers (
                result_uuid, run_idx, marker_idx, timestamp_us, name
            ) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;

        for m in &data.markers {
            stmt.execute(rusqlite::params![
                result_uuid,
                run_idx as i64,
                m.marker_idx,
                m.timestamp_us,
                m.name,
            ])?;
        }
    }

    // Insert summary.
    tx.execute(
        "INSERT INTO sidecar_summary (
            result_uuid, run_idx, vm_hwm_kb, sample_count, marker_count, wall_time_ms
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            result_uuid,
            run_idx as i64,
            data.summary.vm_hwm_kb,
            data.summary.sample_count,
            data.summary.marker_count,
            data.summary.wall_time_ms,
        ],
    )?;

    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_proc_stat_self() {
        // We can always read our own /proc/self/stat.
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
        // rchar should be > 0 since we've read files during the test.
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
        // PID 0 is the kernel scheduler — /proc/0/stat doesn't exist on most systems.
        let stat = read_proc_stat(0);
        assert!(stat.is_none());
    }

    #[test]
    fn monotonic_clock_advances() {
        let t1 = monotonic_ns();
        let t2 = monotonic_ns();
        assert!(t2 >= t1);
    }
}
