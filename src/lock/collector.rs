//! The process sample behind `brokkr lock`: which processes belong to the
//! hold, and what each is doing.
//!
//! Membership has three sources, each with its own blind spots, so the union
//! is reported and every row says which found it:
//!
//! - **token** (`T`): every same-user process whose `/proc/<pid>/environ`
//!   carries this acquisition's `BROKKR_EXEC=<id>.<seq>`, stamped at the spawn
//!   choke point and inherited by everything below - so it survives an
//!   ancestor's exit, a double fork and reparenting. `environ` is the
//!   environment of the process's last exec, so a later in-process change is
//!   invisible to it: membership and attribution are observations.
//! - **descendant** (`D`): the parent-PID walk from the holder, which catches
//!   a child that dropped its environment.
//! - **cgroup** (`C`): members of the hold's cgroup, where one exists and its
//!   PIDs mean the same thing to `/proc` as to the holder.
//!
//! A sample is two scans about [`SAMPLE_GAP`] apart. A row's CPU rate is the
//! change in its CPU ticks between them over the time between its two reads,
//! given only when the same process carried the same token value in both.
//!
//! The holder runs [`Collector`] on its own thread, one collection at a time
//! and no more often than [`MIN_REFRESH`]; the control socket serves the
//! cached sample with its age and never waits on a collection. The
//! reader-side fallback (`brokkr lock` when the holder does not answer) calls
//! [`sample`] directly with the holder's PID as root.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::observe::{EXEC_ENV, Observed, parse_exec, sanitize};
use super::report::{CgroupRow, ProcRow, SampleView};

/// Time between a sample's two scans.
pub(crate) const SAMPLE_GAP: Duration = Duration::from_millis(250);
/// The least time between two collections the holder starts.
const MIN_REFRESH: Duration = Duration::from_millis(750);
/// Most bytes of one environment block inspected.
const MAX_ENVIRON: u64 = 1 << 20;
/// Most bytes of one command line kept.
const MAX_CMDLINE: u64 = 4096;

/// What to sample.
pub(crate) struct SampleSpec<'a> {
    /// The acquisition id whose token marks members, if known.
    pub(crate) acq: Option<&'a str>,
    /// The holder: excluded from rows, root of the descendant walk.
    pub(crate) root: u32,
    /// PIDs from `/proc` mean what they mean to the holder.
    pub(crate) pids_meaningful: bool,
    /// The holder's observation table, for launch labels.
    pub(crate) observed: Option<&'a Observed>,
}

struct Seen {
    stat: crate::process::ProcStat,
    token: Option<String>,
    at: Instant,
}

struct Scan {
    members: HashMap<(u32, u64), (Seen, String)>,
    unreadable: usize,
}

/// Two scans and the rows they describe.
pub(crate) fn sample(spec: &SampleSpec<'_>) -> SampleView {
    let started = Instant::now();
    let cgroup = spec.observed.and_then(Observed::cgroup);
    let cg_first = cgroup.and_then(|c| c.cpu_usec().map(|u| (u, Instant::now())));
    let first = scan(spec, cgroup);
    std::thread::sleep(SAMPLE_GAP);
    let second = scan(spec, cgroup);
    let cg_second = cgroup.and_then(|c| c.cpu_usec().map(|u| (u, Instant::now())));

    let clk = crate::process::clock_ticks_per_second().unwrap_or(100);
    let page_kb = page_size() / 1024;
    let uptime_ms = uptime_ms();

    let mut rows = Vec::new();
    let mut cpu_total = 0u64;
    let mut rss_total = 0u64;
    for ((pid, start), (seen, via)) in &second.members {
        let ticks = seen.stat.user_ticks() + seen.stat.system_ticks();
        let rate = first.members.get(&(*pid, *start)).and_then(|(before, _)| {
            if before.token != seen.token {
                return None;
            }
            let earlier = before.stat.user_ticks() + before.stat.system_ticks();
            let dt = seen.at.saturating_duration_since(before.at).as_secs_f64();
            if dt <= 0.0 || ticks < earlier {
                return None;
            }
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let milli = ((ticks - earlier) as f64 / clk as f64 / dt * 1000.0).round() as u64;
            Some(milli)
        });
        let Some(row) = describe(*pid, seen, via, rate, clk, page_kb, uptime_ms, spec) else {
            continue;
        };
        cpu_total += rate.unwrap_or(0);
        rss_total += row.rss_kb;
        rows.push(row);
    }
    rows.sort_by(|a, b| {
        b.cpu_milli
            .unwrap_or(0)
            .cmp(&a.cpu_milli.unwrap_or(0))
            .then(b.age_ms.cmp(&a.age_ms))
    });
    let cgroup_row = cgroup.map(|c| {
        let cpu_milli = match (cg_first, cg_second) {
            (Some((a, ta)), Some((b, tb))) if b >= a => {
                let dt = tb.saturating_duration_since(ta).as_secs_f64();
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                (dt > 0.0).then(|| ((b - a) as f64 / 1000.0 / dt).round() as u64)
            }
            _ => None,
        };
        CgroupRow {
            name: c.name.clone(),
            since_entry_ms: millis(c.entered.elapsed()),
            cpu_usec: cg_second.map_or(0, |(u, _)| u),
            cpu_milli,
            memory_bytes: c.memory_bytes(),
            members: c.pids().len(),
        }
    });
    SampleView {
        age_ms: 0,
        interval_ms: millis(started.elapsed()),
        pids_meaningful: spec.pids_meaningful,
        observed: rows.len(),
        omitted: 0,
        unreadable: second.unreadable,
        cpu_milli_total: cpu_total,
        rss_kb_total: rss_total,
        rows,
        cgroup: cgroup_row,
    }
}

fn stat_start(stat: &crate::process::ProcStat) -> u64 {
    stat.start().get()
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// One pass over `/proc`.
fn scan(spec: &SampleSpec<'_>, cgroup: Option<&super::cgroup::CgroupView>) -> Scan {
    let my_uid = unsafe { libc::geteuid() };
    let mut stats: HashMap<u32, Seen> = HashMap::new();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    // Same-user processes whose environment could not be checked, with start
    // times in clock ticks since boot.
    let mut unreadable: Vec<(u32, u64)> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Scan {
            members: HashMap::new(),
            unreadable: 0,
        };
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
            .and_then(crate::process::Pid::new)
        else {
            continue;
        };
        let at = Instant::now();
        let Ok(stat) = crate::process::ProcStat::read(pid) else {
            continue;
        };
        let parent = stat.parent().map_or(0, crate::process::Pid::get);
        children.entry(parent).or_default().push(pid.get());
        let same_user = entry.metadata().is_ok_and(|m| m.uid() == my_uid);
        let token = match spec.acq {
            Some(_) if same_user && stat.state() != 'Z' => match exec_token(pid.get()) {
                Environ::Found(value) => Some(value),
                Environ::Absent => None,
                Environ::Unreadable => {
                    unreadable.push((pid.get(), stat_start(&stat)));
                    None
                }
            },
            _ => None,
        };
        stats.insert(pid.get(), Seen { stat, token, at });
    }

    let mut via: HashMap<u32, String> = HashMap::new();
    if let Some(acq) = spec.acq {
        for (pid, seen) in &stats {
            if seen
                .token
                .as_deref()
                .and_then(parse_exec)
                .is_some_and(|(a, _)| a == acq)
            {
                via.entry(*pid).or_default().push('T');
            }
        }
    }
    let mut stack = vec![spec.root];
    let mut visited = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) || visited.len() > 65_536 {
            continue;
        }
        if let Some(kids) = children.get(&pid) {
            for kid in kids {
                via.entry(*kid).or_default().push('D');
                stack.push(*kid);
            }
        }
    }
    if spec.pids_meaningful
        && let Some(cg) = cgroup
    {
        for pid in cg.pids() {
            if stats.contains_key(&pid) {
                via.entry(pid).or_default().push('C');
            }
        }
    }
    via.remove(&spec.root);
    // Only an unreadable process that the token could have been the proof for
    // still matters: one that is already a member (descendant or cgroup) is
    // counted as a row, the holder is not a member of itself, and one that
    // started before the hold began cannot carry a token minted by it. The
    // age cut needs the hold's start, so the reader-side fallback keeps them.
    let cutoff_ms = spec.observed.and_then(|o| {
        let boot_to_now = uptime_ms()?;
        let began = boot_to_now.saturating_sub(millis(o.began().elapsed()));
        Some(began.saturating_sub(1000))
    });
    let clk = crate::process::clock_ticks_per_second().unwrap_or(100).max(1);
    let unreadable = unreadable
        .into_iter()
        .filter(|(pid, start)| {
            *pid != spec.root
                && !via.contains_key(pid)
                && cutoff_ms.is_none_or(|c| start.saturating_mul(1000) / clk >= c)
        })
        .count();
    let members = via
        .into_iter()
        .filter_map(|(pid, how)| {
            let seen = stats.remove(&pid)?;
            Some(((pid, seen.stat.start().get()), (seen, how)))
        })
        .collect();
    Scan {
        members,
        unreadable,
    }
}

/// The rest of a row: command line, IO, age. `None` when the PID changed
/// identity while it was being read.
#[allow(clippy::too_many_arguments)]
fn describe(
    pid: u32,
    seen: &Seen,
    via: &str,
    rate: Option<u64>,
    clk: u64,
    page_kb: u64,
    uptime_ms: Option<u64>,
    spec: &SampleSpec<'_>,
) -> Option<ProcRow> {
    let argv = cmdline(pid).unwrap_or_else(|| format!("[{}]", seen.stat.comm()));
    let io = std::fs::read_to_string(format!("/proc/{pid}/io")).ok();
    let field = |key: &str| {
        io.as_deref().and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|v| v.trim().parse::<u64>().ok())
        })
    };
    let read_bytes = field("read_bytes:");
    let write_bytes = field("write_bytes:");
    // The identity sandwich: the stat row read in the scan must still be
    // this PID's, or the command line and IO above may be another process's.
    let again = crate::process::ProcStat::read(crate::process::Pid::new(pid)?).ok()?;
    if again.start() != seen.stat.start() {
        return None;
    }
    let ticks = seen.stat.user_ticks() + seen.stat.system_ticks();
    let start_ms = seen.stat.start().get().saturating_mul(1000) / clk.max(1);
    let launched = seen.token.as_deref().and_then(parse_exec).map(|(acq, seq)| {
        match spec.observed {
            Some(o) if o.acq().as_str() == acq => match o.launch_record(seq) {
                // The launch's label is its own (shortened) command line, so
                // on the launched process itself only the activity is news;
                // a descendant names the launch it came from.
                Some(r) => match (is_the_launch(&argv, &r.label), r.under) {
                    (true, Some(under)) => format!("under {under}"),
                    (true, None) => "launched directly".to_owned(),
                    (false, Some(under)) => format!("from {} under {under}", r.label),
                    (false, None) => format!("from {}", r.label),
                },
                None => format!("cohort {seq}, label not kept"),
            },
            _ => format!("cohort {seq}"),
        }
    });
    Some(ProcRow {
        pid,
        ppid: seen.stat.parent().map_or(0, crate::process::Pid::get),
        via: via.to_owned(),
        cpu_milli: rate,
        cpu_ms: ticks.saturating_mul(1000) / clk.max(1),
        rss_kb: seen.stat.rss_pages() * page_kb,
        age_ms: uptime_ms.map_or(0, |u| u.saturating_sub(start_ms)),
        state: seen.stat.state(),
        read_bytes,
        write_bytes,
        launched,
        argv,
    })
}

/// Whether a process's command line is the launch a cohort label names. The
/// label (`spawn::launch_label`) is the program's file name and its first
/// few arguments, so the row's argv with its program shortened the same way
/// starts with it. An exec'd wrapper or a forked child does not match and is
/// shown as coming from the launch.
fn is_the_launch(argv: &str, label: &str) -> bool {
    let (program, rest) = argv.split_once(' ').unwrap_or((argv, ""));
    let name = program.rsplit('/').next().unwrap_or(program);
    let shortened = if rest.is_empty() {
        name.to_owned()
    } else {
        format!("{name} {rest}")
    };
    !label.is_empty() && shortened.starts_with(label)
}

enum Environ {
    Found(String),
    Absent,
    Unreadable,
}

/// The `BROKKR_EXEC` value in `/proc/<pid>/environ`, reading at most
/// [`MAX_ENVIRON`] bytes. A block cut off before the marker is found counts
/// as not inspected, never as unmarked.
fn exec_token(pid: u32) -> Environ {
    let Ok(file) = std::fs::File::open(format!("/proc/{pid}/environ")) else {
        return Environ::Unreadable;
    };
    let mut buf = Vec::new();
    if file.take(MAX_ENVIRON + 1).read_to_end(&mut buf).is_err() {
        return Environ::Unreadable;
    }
    let cut = buf.len() as u64 > MAX_ENVIRON;
    let prefix = format!("{EXEC_ENV}=");
    for entry in buf.split(|b| *b == 0) {
        if let Some(value) = entry.strip_prefix(prefix.as_bytes()) {
            return match std::str::from_utf8(value) {
                Ok(v) => Environ::Found(v.to_owned()),
                Err(_) => Environ::Unreadable,
            };
        }
    }
    if cut { Environ::Unreadable } else { Environ::Absent }
}

fn cmdline(pid: u32) -> Option<String> {
    let file = std::fs::File::open(format!("/proc/{pid}/cmdline")).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_CMDLINE).read_to_end(&mut buf).ok()?;
    if buf.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&buf);
    let joined = text.split('\0').filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
    Some(sanitize(&joined).chars().take(300).collect())
}

fn page_size() -> u64 {
    // SAFETY: constant selector.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(size).unwrap_or(4096).max(1024)
}

fn uptime_ms() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((secs * 1000.0) as u64)
}

/// The holder's PID as its own `/proc` numbers it, and whether that agrees
/// with `getpid` - when it does not, `/proc` belongs to another PID namespace
/// and its numbers are shown to nobody.
pub(crate) fn holder_view() -> (u32, bool) {
    let own = std::process::id();
    match crate::process::ProcStat::read_self() {
        Ok(stat) => {
            let seen = stat.identity().pid().get();
            (seen, seen == own)
        }
        Err(_) => (own, false),
    }
}

/// The holder-side collector: single-flight, rate-limited, cached.
pub(crate) struct Collector {
    observed: Arc<Observed>,
    cache: Mutex<Option<(SampleView, Instant)>>,
    busy: AtomicBool,
    last_start: Mutex<Option<Instant>>,
}

impl Collector {
    pub(crate) fn new(observed: Arc<Observed>) -> Self {
        Self {
            observed,
            cache: Mutex::new(None),
            busy: AtomicBool::new(false),
            last_start: Mutex::new(None),
        }
    }

    /// Start a collection on its own thread unless one is running or one
    /// started less than [`MIN_REFRESH`] ago. Never blocks.
    pub(crate) fn kick(self: &Arc<Self>) {
        if self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let mut last = self.last_start.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if last.is_some_and(|t| t.elapsed() < MIN_REFRESH) {
                self.busy.store(false, Ordering::Release);
                return;
            }
            *last = Some(Instant::now());
        }
        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("brokkr-lock-collector".into())
            .spawn(move || {
                let (root, meaningful) = holder_view();
                let acq = me.observed.acq().as_str().to_owned();
                let view = sample(&SampleSpec {
                    acq: Some(&acq),
                    root,
                    pids_meaningful: meaningful,
                    observed: Some(&me.observed),
                });
                *me.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some((view, Instant::now()));
                me.busy.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.busy.store(false, Ordering::Release);
        }
    }

    pub(crate) fn collecting(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    /// The latest sample with its age filled in, without waiting.
    pub(crate) fn cached(&self) -> Option<SampleView> {
        let cache = self.cache.try_lock().ok()?;
        let (view, at) = cache.as_ref()?;
        let mut view = view.clone();
        view.age_ms = millis(at.elapsed());
        Some(view)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The launched process matches its own label (program shortened to its
    /// file name, first arguments); a child it started does not.
    #[test]
    fn the_launch_is_told_from_its_children() {
        let label = "all-e54d --skip tier2:: --skip";
        assert!(is_the_launch(
            "/target/debug/out/all-e54d --skip tier2:: --skip tier3:: --test-threads=2",
            label
        ));
        assert!(!is_the_launch(
            "/target/debug/pbfhogg add-locations-to-ways in.pbf -o out.pbf",
            label
        ));
        assert!(is_the_launch("/usr/bin/sleep", "sleep"));
        assert!(!is_the_launch("/usr/bin/sleep", ""));
    }

    /// A child stamped with a token is found by the token scan even though the
    /// sample is rooted somewhere that is not its ancestor.
    #[test]
    fn a_token_carrier_is_observed_and_rated() {
        let acq = "fedcba9876543210fedcba9876543210";
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .env(EXEC_ENV, format!("{acq}.7"))
            .spawn()
            .unwrap();
        // Wait until the exec has happened and the environment is visible.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(exec_token(child.id()), Environ::Found(_)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let view = sample(&SampleSpec {
            acq: Some(acq),
            // No such process: nothing is a descendant, so only the token
            // can have found the child.
            root: u32::MAX,
            pids_meaningful: true,
            observed: None,
        });
        child.kill().ok();
        child.wait().ok();
        let row = view.rows.iter().find(|r| r.pid == child.id()).unwrap();
        assert_eq!(row.via, "T", "{row:?}");
        assert_eq!(row.launched.as_deref(), Some("cohort 7"));
        assert!(row.argv.starts_with("sleep 5"), "{row:?}");
        assert!(row.cpu_milli.is_some(), "present in both scans with the same token");
    }

    #[test]
    fn descendants_of_the_root_are_observed_without_a_token() {
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        let view = sample(&SampleSpec {
            acq: None,
            root: std::process::id(),
            pids_meaningful: true,
            observed: None,
        });
        child.kill().ok();
        child.wait().ok();
        let row = view.rows.iter().find(|r| r.pid == child.id()).unwrap();
        assert!(row.via.contains('D'));
    }
}
