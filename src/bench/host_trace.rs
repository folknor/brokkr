//! The host's memory writeback and scratch-device state around and through
//! one script-bench iteration, and the cell cursor that names the iteration
//! before it on this host.
//!
//! Every reading is stamped with `CLOCK_BOOTTIME`, read just after it, so
//! readings of one boot order and subtract across iterations, arms and
//! invocations. The device is the block device holding the iteration
//! container, resolved through sysfs; counters are cumulative and only a
//! reader derives deltas, never across a boot or a device instance change.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::bench::store::{
    Cell, Cursor, Device, DeviceCounters, DeviceLayer, PreviousCommand, Reading,
};

/// How often the sampler reads, as the sidecar samples a child.
const INTERVAL: Duration = crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES.sample_interval;

/// The periodic readings one run may keep across its iterations.
pub(crate) const RUN_READINGS: usize = 100_000;

/// Microseconds since boot, suspend included: the clock every reading uses.
pub(crate) fn boottime_us() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec for the call's duration.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &raw mut ts) } != 0 {
        return 0;
    }
    ts.tv_sec
        .saturating_mul(1_000_000)
        .saturating_add(ts.tv_nsec / 1_000)
}

pub(crate) fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// The block device holding `path`: the whole disk when the filesystem is
/// on a partition, else the filesystem's own node. `None` for a filesystem
/// with no single block device behind its `st_dev` (btrfs, overlay, tmpfs).
pub(crate) fn resolve_device(path: &Path) -> Option<Device> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(path).ok()?.dev();
    let (major, minor) = (libc::major(dev), libc::minor(dev));
    if major == 0 {
        return None;
    }
    let node = std::fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")).ok()?;
    let (node, major, minor) = if node.join("partition").exists() {
        let parent = node.parent()?.to_path_buf();
        let (pmaj, pmin) = dev_numbers(&parent)?;
        (parent, pmaj, pmin)
    } else {
        (node, major, minor)
    };
    let name = node.file_name()?.to_str()?.to_owned();
    let layer = if node
        .join("slaves")
        .read_dir()
        .is_ok_and(|mut d| d.next().is_some())
        || name.starts_with("loop")
    {
        DeviceLayer::Mapped
    } else {
        DeviceLayer::Disk
    };
    let diskseq = std::fs::read_to_string(node.join("diskseq"))
        .ok()
        .and_then(|s| s.trim().parse().ok());
    Some(Device {
        name,
        major,
        minor,
        layer,
        diskseq,
    })
}

/// A sysfs block node's `dev` file, `MAJOR:MINOR`.
fn dev_numbers(node: &Path) -> Option<(u32, u32)> {
    let text = std::fs::read_to_string(node.join("dev")).ok()?;
    let (major, minor) = text.trim().split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// `/proc/meminfo`'s `Dirty` and `Writeback`, each absent when unread.
fn writeback() -> (Option<i64>, Option<i64>) {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else {
        return (None, None);
    };
    parse_meminfo(&text)
}

pub(crate) fn parse_meminfo(text: &str) -> (Option<i64>, Option<i64>) {
    let field = |prefix: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(prefix))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|v| v.parse::<i64>().ok())
    };
    (field("Dirty:"), field("Writeback:"))
}

/// The counters of a `/sys/class/block/NAME/stat` line: fields 5 to 9 and,
/// where the kernel has them, 14 (`Documentation/block/stat.rst`).
pub(crate) fn parse_stat(text: &str) -> Option<DeviceCounters> {
    let fields: Vec<i64> = text
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if fields.len() < 11 {
        return None;
    }
    Some(DeviceCounters {
        writes_completed: fields[4],
        sectors_written: fields[6],
        write_ticks_ms: fields[7],
        in_flight: fields[8],
        sectors_discarded: fields.get(13).copied(),
    })
}

/// One reading now: meminfo, then the device, then the clock. The device's
/// counters are kept only while its name still holds the instance resolved:
/// its numbers and, where it has one, its `diskseq` unchanged.
pub(crate) fn read(device: Option<&Device>) -> Reading {
    let (dirty_kb, writeback_kb) = writeback();
    let device = device.and_then(|d| {
        let node = PathBuf::from(format!("/sys/class/block/{}", d.name));
        let stat = std::fs::read_to_string(node.join("stat")).ok()?;
        let same = dev_numbers(&node) == Some((d.major, d.minor))
            && d.diskseq.is_none_or(|seq| {
                std::fs::read_to_string(node.join("diskseq"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    == Some(seq)
            });
        same.then(|| parse_stat(&stat)).flatten()
    });
    Reading {
        at_us: boottime_us(),
        dirty_kb,
        writeback_kb,
        device,
    }
}

/// The periodic sampler: a thread reading every [`INTERVAL`] until stopped.
pub(crate) struct Sampler {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<std::thread::JoinHandle<(Vec<Reading>, bool)>>,
}

impl Sampler {
    /// Start sampling `device`, keeping at most `budget` readings.
    pub(crate) fn start(device: Option<Device>, budget: usize) -> Self {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("host-trace".to_owned())
            .spawn(move || {
                let (lock, wake) = &*flag;
                let mut readings = Vec::new();
                let mut capped = false;
                let Ok(mut stopped) = lock.lock() else {
                    return (readings, capped);
                };
                loop {
                    // A stop signalled before this thread first took the
                    // lock has already notified: never wait past it.
                    if *stopped {
                        return (readings, capped);
                    }
                    let Ok((guard, timeout)) = wake.wait_timeout(stopped, INTERVAL) else {
                        return (readings, capped);
                    };
                    stopped = guard;
                    if *stopped {
                        return (readings, capped);
                    }
                    if !timeout.timed_out() {
                        continue;
                    }
                    if readings.len() < budget {
                        readings.push(read(device.as_ref()));
                    } else {
                        capped = true;
                    }
                }
            })
            .ok();
        Self { stop, thread }
    }

    /// Stop and join: the readings kept and whether any were dropped.
    pub(crate) fn finish(mut self) -> (Vec<Reading>, bool) {
        self.signal();
        self.thread
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_default()
    }

    fn signal(&self) {
        let (lock, wake) = &*self.stop;
        if let Ok(mut stopped) = lock.lock() {
            *stopped = true;
        }
        wake.notify_all();
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.signal();
    }
}

/// Where the cursor lives: beside the global lock, which every writer of it
/// holds.
fn cursor_path() -> Option<PathBuf> {
    crate::lockfile::cell_cursor_path()
}

/// The cursor as it stands; `None` when there is none or it does not read.
pub(crate) fn read_cursor() -> Option<Cursor> {
    let text = std::fs::read_to_string(cursor_path()?).ok()?;
    serde_json::from_str(&text).ok()
}

/// Replace the cursor whole: written beside it, then renamed over it.
pub(crate) fn write_cursor(cursor: &Cursor) -> Result<(), String> {
    let path = cursor_path().ok_or("no home to keep the bench cell cursor in")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let staging = path.with_extension("json.tmp");
    let text = serde_json::to_vec(cursor).map_err(|e| e.to_string())?;
    std::fs::write(&staging, text).map_err(|e| e.to_string())?;
    std::fs::rename(&staging, &path).map_err(|e| e.to_string()) // gate: brokkr's own state, no launch uses it
}

/// Remove the cursor, so no later cell names an older one across this one:
/// whether none is left.
pub(crate) fn clear_cursor() -> bool {
    let Some(path) = cursor_path() else {
        return false;
    };
    let removed = std::fs::remove_file(path); // gate: brokkr's own state, no launch uses it
    match removed {
        Ok(()) => true,
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Which iteration a cursor names.
pub(crate) fn cell(run: &str, project: &str, bench: &str, iteration: usize) -> Cell {
    Cell {
        run: run.to_owned(),
        project: project.to_owned(),
        bench: bench.to_owned(),
        iteration: u32::try_from(iteration).unwrap_or(u32::MAX),
        boot_id: boot_id(),
    }
}

/// When `fstrim.timer` last triggered: its persistent stamp's mtime.
pub(crate) fn fstrim_timer_unix_ms() -> Option<i64> {
    let modified = std::fs::metadata("/var/lib/systemd/timers/stamp-fstrim.timer")
        .ok()?
        .modified()
        .ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

/// The newest row of this user's command history.
pub(crate) fn previous_command() -> Option<PreviousCommand> {
    crate::history::HistoryDb::open()
        .ok()?
        .latest()
        .ok()
        .flatten()
}

/// What a window of readings shows: sampled gauges, and device deltas
/// between its first and last reading that carry counters.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Window {
    pub readings: usize,
    pub peak_dirty_kb: Option<i64>,
    pub peak_writeback_kb: Option<i64>,
    pub peak_in_flight: Option<i64>,
    /// Absent with fewer than two device readings, or across a counter
    /// regression (a reset). Whole KiB, an odd sector rounded down.
    pub written_kb: Option<i64>,
    pub discarded_kb: Option<i64>,
    /// Mean block-layer write wait per completed request, queueing
    /// included; absent when no write completed.
    pub write_wait_ms: Option<f64>,
}

/// Every reading of `trace` stamped within `[from_us, to_us]`, boundary
/// readings included, summarized.
pub(crate) fn window(trace: &crate::bench::store::HostTrace, from_us: i64, to_us: i64) -> Window {
    let all = trace
        .start
        .iter()
        .chain(&trace.readings)
        .chain(&trace.end)
        .filter(|r| r.at_us >= from_us && r.at_us <= to_us);
    summarize(all)
}

pub(crate) fn summarize<'a>(readings: impl Iterator<Item = &'a Reading>) -> Window {
    let mut w = Window::default();
    let mut first: Option<DeviceCounters> = None;
    let mut last: Option<DeviceCounters> = None;
    let mut regressed = false;
    let mut counted = 0_usize;
    let mut last_discarded: Option<i64> = None;
    for r in readings {
        w.readings += 1;
        w.peak_dirty_kb = w.peak_dirty_kb.max(r.dirty_kb);
        w.peak_writeback_kb = w.peak_writeback_kb.max(r.writeback_kb);
        if let Some(d) = r.device {
            w.peak_in_flight = w.peak_in_flight.max(Some(d.in_flight));
            if let Some(prev) = last
                && (d.sectors_written < prev.sectors_written
                    || d.writes_completed < prev.writes_completed
                    || d.write_ticks_ms < prev.write_ticks_ms)
            {
                regressed = true;
            }
            // Against the last reading that had one, so a reading without
            // discards between two cannot hide a reset.
            if let Some(now) = d.sectors_discarded {
                if last_discarded.is_some_and(|before| now < before) {
                    regressed = true;
                }
                last_discarded = Some(now);
            }
            first.get_or_insert(d);
            last = Some(d);
            counted += 1;
        }
    }
    if let (Some(a), Some(b)) = (first, last)
        && !regressed
        && counted > 1
    {
        apply_delta(&mut w, &a, &b);
    }
    w
}

fn apply_delta(w: &mut Window, a: &DeviceCounters, b: &DeviceCounters) {
    w.written_kb = Some((b.sectors_written - a.sectors_written) / 2);
    w.discarded_kb = a
        .sectors_discarded
        .zip(b.sectors_discarded)
        .map(|(x, y)| (y - x) / 2);
    let writes = b.writes_completed - a.writes_completed;
    #[allow(clippy::cast_precision_loss)] // a display mean
    {
        w.write_wait_ms =
            (writes > 0).then(|| (b.write_ticks_ms - a.write_ticks_ms) as f64 / writes as f64);
    }
}

/// The gap from the previous recorded cell end to an iteration's start.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Gap {
    /// No cursor was found.
    None,
    /// The previous cell started and never recorded its end.
    Unended { cell: Cell },
    /// Another boot, an unknown one, or an end stamped after this start: no
    /// elapsed time or delta applies.
    OtherBoot { cell: Cell },
    Ended {
        cell: Cell,
        elapsed_us: i64,
        release_completed: bool,
        /// Device deltas over the gap; absent when the device instance is
        /// not the same one (or not shown to be, without a diskseq on both)
        /// or the counters went backwards.
        device: Option<Window>,
    },
}

pub(crate) fn gap(trace: &crate::bench::store::HostTrace) -> Gap {
    let Some(previous) = &trace.previous else {
        return Gap::None;
    };
    let Some(start) = &trace.start else {
        return Gap::None;
    };
    match previous {
        Cursor::InProgress { cell } => Gap::Unended { cell: cell.clone() },
        Cursor::Ended {
            cell,
            device,
            reading,
            release_completed,
        } => {
            // Validation refuses a same-boot predecessor ending after this
            // start; a reversed pair is treated as another boot all the same.
            if cell.boot_id.is_none()
                || cell.boot_id != trace.boot_id
                || reading.at_us > start.at_us
            {
                return Gap::OtherBoot { cell: cell.clone() };
            }
            let same_device = match (device, &trace.device) {
                (Some(a), Some(b)) => {
                    a.name == b.name
                        && a.major == b.major
                        && a.minor == b.minor
                        && a.diskseq.is_some()
                        && a.diskseq == b.diskseq
                }
                _ => false,
            };
            let device = same_device
                .then(|| summarize([reading, start].into_iter()))
                .filter(|w| w.written_kb.is_some());
            Gap::Ended {
                cell: cell.clone(),
                elapsed_us: start.at_us - reading.at_us,
                release_completed: *release_completed,
                device,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn stat_line_parses_write_counters_and_discards() {
        let line = "  100 2 300 4   50 6 7000 80 3 90 100 9 1 4096 5 0 0";
        let c = parse_stat(line).unwrap();
        assert_eq!(
            (
                c.writes_completed,
                c.sectors_written,
                c.write_ticks_ms,
                c.in_flight
            ),
            (50, 7000, 80, 3)
        );
        assert_eq!(c.sectors_discarded, Some(4096));
        let old = "100 2 300 4 50 6 7000 80 3 90 100";
        assert_eq!(parse_stat(old).unwrap().sectors_discarded, None);
        assert!(parse_stat("1 2 3").is_none());
    }

    #[test]
    fn meminfo_reads_dirty_and_writeback() {
        let text = "MemTotal: 100 kB\nDirty:  12 kB\nWriteback:   0 kB\nWritebackTmp: 5 kB\n";
        assert_eq!(parse_meminfo(text), (Some(12), Some(0)));
        assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), (None, None));
    }

    #[test]
    fn a_filesystem_without_a_block_device_resolves_none() {
        assert_eq!(resolve_device(Path::new("/proc")), None);
    }

    #[test]
    fn sampler_stops_promptly_and_respects_budget() {
        let sampler = Sampler::start(None, 0);
        std::thread::sleep(INTERVAL * 3);
        let (readings, capped) = sampler.finish();
        assert!(readings.is_empty());
        assert!(capped);
    }
}
