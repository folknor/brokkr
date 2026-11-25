//! Per-thread `/proc` evidence for a timed-out test run, collected off the
//! kill path.
//!
//! No `/proc` read may sit between a deadline verdict and its signals:
//! `stat` and `syscall` can sleep on a target's exec lock, and `syscall` waits
//! for the target to go inactive. So every read happens on a collector thread
//! that sends finished rows over its own channel, and the deadline owner only
//! sends a request and drains the channel against an absolute cutoff. Closing a
//! pass drops its receiver, which discards late rows; a collector stuck in a
//! read blocks only itself. A process-wide dispatcher starts the collectors, so
//! the owner never waits on a thread spawn either.
//!
//! What is read is evidence, not an inventory: a descendant can escape the
//! tree, and every row is read at its own moment. Each process is read
//! through a handle on its `/proc/<pid>` directory and each thread through a
//! handle on its task directory, so no read reaches a later process or thread
//! reusing the number. The root's handle is opened at spawn, while the leader
//! is an unreaped child; a child's is opened when its parent names it, and
//! the child is kept only if its own `stat` names that parent. Every read
//! that fails other than by the target being gone is counted, so a pass with
//! gaps never reads as complete.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

/// Most tasks one pass reads; a tree beyond this is recorded as truncated.
const TASK_CAP: usize = 4096;
/// Most bytes kept from one `/proc` file; a file that reaches it counts as a
/// gap, since its tail is missing.
const READ_CAP: usize = 64 * 1024;
/// Most collector threads alive at once in this process. One stuck in a
/// kernel read holds its permit for good; at the cap, a pass is unavailable.
const COLLECTOR_CAP: usize = 8;
/// How far ahead of the nearest deadline the watchdog takes its baseline, and
/// the least time between two baselines.
pub(super) const PRESAMPLE_LEAD: Duration = Duration::from_secs(5);
/// The live pass closes this long after the verdict, then the stop is sent.
pub(super) const LIVE_ALLOWANCE: Duration = Duration::from_millis(250);
/// The post-stop-request pass closes this long after the verdict, then the
/// kill is sent.
pub(super) const STOP_ALLOWANCE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pass {
    /// The baseline taken ahead of the deadline.
    Presample,
    /// At the verdict, before the stop: the only pass whose wait channels say
    /// what a task waited on, since a stopped task's is the stop itself.
    Live,
    /// After the stop request.
    PostStop,
}

impl Pass {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Presample => "presample",
            Self::Live => "live",
            Self::PostStop => "post-stop-request",
        }
    }
}

/// A process a report may name threads of, by pid and, where brokkr read it
/// at admission, start time. `start: None` is the walk's own root, whose rows
/// all come through its pinned [`ProcDir`] and so are that process's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Root {
    pub(crate) pid: u32,
    pub(crate) start: Option<u64>,
}

/// A handle on one process's `/proc/<pid>` directory, opened while the
/// process is known to be the one meant (an unreaped child). Opening a
/// directory takes no lock of the target's. Every read through the handle
/// is that process's or fails once it is gone; a later process reusing the
/// pid is never reached through it.
#[derive(Debug)]
pub(crate) struct ProcDir {
    pid: u32,
    fd: OwnedFd,
}

/// Whether an error means the process or task is gone, as opposed to a read
/// brokkr could not make.
fn gone(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH))
}

fn open_dir(dirfd: Option<&OwnedFd>, path: &str) -> std::io::Result<OwnedFd> {
    let c = CString::new(path).map_err(|_| std::io::Error::other("nul in path"))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // SAFETY: a valid NUL-terminated path; `dirfd` is a valid open fd or
    // AT_FDCWD for an absolute path.
    let fd = unsafe {
        libc::openat(
            dirfd.map_or(libc::AT_FDCWD, AsRawFd::as_raw_fd),
            c.as_ptr(),
            flags,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a successful openat returns a fresh fd we uniquely own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

impl ProcDir {
    pub(crate) fn open(pid: u32) -> std::io::Result<Self> {
        Ok(Self {
            pid,
            fd: open_dir(None, &format!("/proc/{pid}"))?,
        })
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// This process's thread ids, the main thread first whatever its
    /// number: its row is the one the process is checked on before any
    /// other thread is sent.
    fn tids(&self) -> std::io::Result<(Vec<u32>, usize)> {
        let task = open_dir(Some(&self.fd), "task")?;
        // The fd's magic link resolves to the pinned directory itself.
        let entries = fs::read_dir(format!("/proc/self/fd/{}", task.as_raw_fd()))?;
        let mut gaps = 0;
        let mut tids: Vec<u32> = Vec::new();
        for entry in entries {
            match entry {
                Ok(e) => tids.extend(
                    e.file_name()
                        .to_str()
                        .and_then(|n| n.parse::<u32>().ok()),
                ),
                Err(_) => gaps += 1,
            }
        }
        let pid = self.pid;
        tids.sort_unstable_by_key(|tid| (*tid != pid, *tid));
        Ok((tids, gaps))
    }

    fn task(&self, tid: u32) -> std::io::Result<TaskDir> {
        open_dir(Some(&self.fd), &format!("task/{tid}")).map(TaskDir)
    }
}

/// One thread's readings. Every field is `None` when its file was unreadable.
#[derive(Clone, Debug, Default)]
pub(crate) struct TaskRow {
    pub(crate) pid: u32,
    pub(crate) tid: u32,
    /// The process this one was found under, `None` for the walk's root.
    pub(crate) parent: Option<u32>,
    /// The kernel's `comm` bytes, at most 15, not necessarily UTF-8.
    pub(crate) comm: Option<Vec<u8>>,
    pub(crate) state: Option<char>,
    pub(crate) ppid: Option<u32>,
    pub(crate) pgid: Option<u32>,
    /// Clock ticks since boot at task start; with `tid`, the task's identity.
    pub(crate) start: Option<u64>,
    pub(crate) wchan: Option<String>,
    /// `/proc/.../syscall` as read: a number and registers, `running`, or `-1`.
    pub(crate) syscall: Option<String>,
    /// From `schedstat`: on-CPU ns and runqueue-wait ns, as accounted so far.
    pub(crate) on_cpu_ns: Option<u64>,
    pub(crate) runqueue_ns: Option<u64>,
    /// When `schedstat` was read, relative to the pass request: the moment
    /// the two counters above describe.
    pub(crate) sched_at: Option<Duration>,
    pub(crate) voluntary: Option<u64>,
    pub(crate) involuntary: Option<u64>,
    /// When `status` was read, for the switch counters and allowed CPUs.
    pub(crate) status_at: Option<Duration>,
    pub(crate) cpus_allowed: Option<String>,
    /// Monotonic stamps around this row's reads, relative to the pass request.
    pub(crate) read_start: Duration,
    pub(crate) read_end: Duration,
}

impl TaskRow {
    pub(crate) fn same_task(&self, other: &Self) -> bool {
        self.tid == other.tid && self.start.is_some() && self.start == other.start
    }

    /// `comm` for display: lossy, with control characters replaced so a row
    /// stays one line of one column.
    pub(crate) fn comm_text(&self) -> String {
        self.comm.as_deref().map_or_else(
            || "?".to_owned(),
            |c| {
                String::from_utf8_lossy(c)
                    .chars()
                    .map(|ch| if ch.is_control() { '?' } else { ch })
                    .collect()
            },
        )
    }
}

/// System-wide readings, each stamped when it was read.
#[derive(Clone, Debug, Default)]
pub(crate) struct HostRow {
    pub(crate) loadavg: Option<String>,
    /// `/proc/pressure/{cpu,io,memory}` `some` totals in microseconds, when
    /// the kernel exposes PSI, each with the moment it was read.
    pub(crate) pressure: Vec<(&'static str, u64, Duration)>,
}

enum Msg {
    /// The collector is running: a disconnect from here on is a death.
    Started,
    Row(Box<TaskRow>),
    Host(HostRow),
    End {
        truncated: bool,
        gaps: usize,
        root_gone: bool,
        /// When the walk's last read ended, relative to the request.
        at: Duration,
    },
}

/// How one pass ended, as the owner saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Coverage {
    /// The collector read the whole tree it found, with no failed read.
    Complete,
    /// The walk finished, but `gaps` reads failed or were cut short.
    Partial { gaps: usize },
    /// The collector found more tasks than the cap; `gaps` as for `Partial`.
    Truncated { gaps: usize },
    /// The leader was gone before the walk reached it, so nothing was read.
    RootGone,
    /// The leader could not be pinned at spawn, so no pass can read it.
    NoRoot,
    /// The pass closed at its cutoff with the collector still reading.
    CutOff,
    /// No collector could start: the dispatcher was missing or every permit
    /// was held.
    Unavailable,
    /// The collector ended without its terminal record.
    Died,
}

impl Coverage {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Complete => "complete".into(),
            Self::Partial { gaps } => format!("partial ({gaps} reads failed or were cut short)"),
            Self::Truncated { gaps: 0 } => "truncated at the task cap".into(),
            Self::Truncated { gaps } => {
                format!("truncated at the task cap, {gaps} reads failed or were cut short")
            }
            Self::RootGone => "not read (the leader was already gone)".into(),
            Self::NoRoot => "not read (the leader's /proc directory could not be opened at spawn)".into(),
            Self::CutOff => "cut off at its allowance".into(),
            Self::Unavailable => "unavailable (no collector could start)".into(),
            Self::Died => "collector ended early".into(),
        }
    }

    /// Whether the pass read every task it found.
    pub(crate) fn whole(&self) -> bool {
        matches!(self, Self::Complete)
    }

    /// Whether the pass finished its walk, gaps or not: a record fit to be a
    /// baseline.
    fn finished(&self) -> bool {
        matches!(
            self,
            Self::Complete | Self::Partial { .. } | Self::Truncated { .. }
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PassRecord {
    pub(crate) pass: Pass,
    /// When the pass was requested; row stamps count from here.
    pub(crate) requested: Instant,
    pub(crate) rows: Vec<TaskRow>,
    pub(crate) host: Option<HostRow>,
    pub(crate) coverage: Coverage,
    /// When the collector's last read ended, from its terminal record;
    /// `None` for a pass that never sent one.
    pub(crate) ended: Option<Instant>,
}

impl PassRecord {
    fn empty(pass: Pass, requested: Instant, coverage: Coverage) -> Self {
        Self {
            pass,
            requested,
            rows: Vec::new(),
            host: None,
            coverage,
            ended: None,
        }
    }

    /// The row in this pass for the same task as `row`, by tid and start time.
    pub(crate) fn matching(&self, row: &TaskRow) -> Option<&TaskRow> {
        self.rows.iter().find(|r| r.same_task(row))
    }

    /// The start time of process `pid` as this pass read it: its main
    /// thread's.
    pub(crate) fn process_start(&self, pid: u32) -> Option<u64> {
        self.rows
            .iter()
            .find(|r| r.pid == pid && r.tid == pid)
            .and_then(|r| r.start)
    }
}

struct Request {
    root: Arc<ProcDir>,
    requested: Instant,
    cutoff: Instant,
    reply: Sender<Msg>,
}

static DISPATCHER: OnceLock<Option<Sender<Request>>> = OnceLock::new();
static ACTIVE_COLLECTORS: AtomicUsize = AtomicUsize::new(0);

/// Start the process-wide dispatcher if it is not running. Called when a
/// watchdog starts, never at a verdict.
pub(super) fn ensure_dispatcher() {
    DISPATCHER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Request>();
        std::thread::Builder::new()
            .name("brokkr-evidence".into())
            .spawn(move || dispatch(&rx))
            .ok()
            .map(|_| tx)
    });
}

fn dispatch(rx: &Receiver<Request>) {
    while let Ok(request) = rx.recv() {
        // A request whose pass already closed would only take a permit from
        // a current one.
        if Instant::now() >= request.cutoff {
            continue;
        }
        let Some(permit) = Permit::acquire() else {
            continue;
        };
        let spawned = std::thread::Builder::new()
            .name("brokkr-collector".into())
            .spawn(move || {
                let _permit = permit;
                collect(&request);
            });
        // A failed spawn drops the closure, and with it the permit and the
        // reply sender: the owner sees the pass as unavailable.
        drop(spawned);
    }
}

struct Permit;

impl Permit {
    fn acquire() -> Option<Self> {
        ACTIVE_COLLECTORS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < COLLECTOR_CAP).then_some(n + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        ACTIVE_COLLECTORS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Post a pass request: an unbounded channel's send, which never waits on the
/// dispatcher. With no dispatcher the reply sender is dropped at once and the
/// pass reads as unavailable.
fn request(root: Arc<ProcDir>, cutoff: Instant) -> (Instant, Receiver<Msg>) {
    let requested = Instant::now();
    let (reply, rx) = mpsc::channel();
    if let Some(Some(sender)) = DISPATCHER.get() {
        // A failed send drops the reply sender: the pass reads unavailable.
        sender
            .send(Request {
                root,
                requested,
                cutoff,
                reply,
            })
            .ok();
    }
    (requested, rx)
}

/// Request a pass and drain it until its terminal record or `cutoff`,
/// whichever is first. The cutoff is checked before every receive.
pub(super) fn run_pass(root: Option<&Arc<ProcDir>>, pass: Pass, cutoff: Instant) -> PassRecord {
    let Some(root) = root else {
        return PassRecord::empty(pass, Instant::now(), Coverage::NoRoot);
    };
    let (requested, rx) = request(Arc::clone(root), cutoff);
    let mut record = PassRecord::empty(pass, requested, Coverage::CutOff);
    let mut started = false;
    loop {
        let now = Instant::now();
        if now >= cutoff {
            break;
        }
        match rx.recv_timeout(cutoff - now) {
            Ok(msg) => {
                started = true;
                if absorb(&mut record, msg) {
                    return record;
                }
            }
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => {
                record.coverage = if started {
                    Coverage::Died
                } else {
                    Coverage::Unavailable
                };
                return record;
            }
        }
    }
    if !started {
        record.coverage = Coverage::Unavailable;
    }
    record
}

/// Fold one message into a record; true at the terminal record.
fn absorb(record: &mut PassRecord, msg: Msg) -> bool {
    match msg {
        Msg::Started => {}
        Msg::Row(row) => record.rows.push(*row),
        Msg::Host(host) => record.host = Some(host),
        Msg::End {
            truncated,
            gaps,
            root_gone,
            at,
        } => {
            record.ended = Some(record.requested + at);
            record.coverage = if root_gone {
                Coverage::RootGone
            } else if truncated {
                Coverage::Truncated { gaps }
            } else if gaps > 0 {
                Coverage::Partial { gaps }
            } else {
                Coverage::Complete
            };
            return true;
        }
    }
    false
}

/// A handle on one task's `/proc` directory. Files read through it come from
/// that task or fail; they never come from a later task reusing the tid.
struct TaskDir(OwnedFd);

impl TaskDir {
    /// Read `name` under this task, up to [`READ_CAP`] bytes. `Err(true)`
    /// means the file reached the cap and its tail is missing.
    fn read(&self, name: &str) -> Result<Vec<u8>, bool> {
        let name = CString::new(name).map_err(|_| false)?;
        // SAFETY: a valid directory fd and NUL-terminated relative name.
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(false);
        }
        // SAFETY: a successful openat returns a fresh fd we uniquely own.
        let file = fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let mut bytes = Vec::new();
        let cap = u64::try_from(READ_CAP + 1).unwrap_or(u64::MAX);
        file.take(cap).read_to_end(&mut bytes).map_err(|_| false)?;
        if bytes.len() > READ_CAP {
            return Err(true);
        }
        Ok(bytes)
    }

    fn text(&self, name: &str, gaps: &mut usize) -> Option<String> {
        match self.read(name) {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Err(_) => {
                *gaps += 1;
                None
            }
        }
    }
}

fn collect(request: &Request) {
    let send = |msg| request.reply.send(msg).is_ok();
    if !send(Msg::Started) {
        return;
    }
    if !send(Msg::Host(read_host(request.requested))) {
        return;
    }
    let mut walk = Walk {
        requested: request.requested,
        gaps: 0,
        count: 0,
        truncated: false,
        // Pids waiting to be walked, with the parent that named them. Only
        // the process being walked holds a directory handle; a child's is
        // opened when it is popped, and checked against its parent then.
        pending: Vec::new(),
        processes: HashSet::new(),
        tasks: HashSet::new(),
    };
    let root = Arc::clone(&request.root);
    match walk.process(&root, None, &send) {
        Visit::Done => {}
        Visit::Closed => return,
        Visit::RootGone => {
            send(Msg::End {
                truncated: false,
                gaps: walk.gaps,
                root_gone: true,
                at: request.requested.elapsed(),
            });
            return;
        }
    }
    while let Some((pid, parent)) = walk.pending.pop() {
        if walk.truncated {
            break;
        }
        let proc_dir = match ProcDir::open(pid) {
            Ok(dir) => dir,
            Err(e) if gone(&e) => continue,
            Err(_) => {
                walk.gaps += 1;
                continue;
            }
        };
        if let Visit::Closed = walk.process(&proc_dir, Some(parent), &send) {
            return;
        }
    }
    // A closed pass has no one to tell.
    send(Msg::End {
        truncated: walk.truncated,
        gaps: walk.gaps,
        root_gone: false,
        at: request.requested.elapsed(),
    });
}

enum Visit {
    Done,
    /// The pass closed; stop reading.
    Closed,
    /// The root's own main thread is gone.
    RootGone,
}

struct Walk {
    requested: Instant,
    gaps: usize,
    count: usize,
    truncated: bool,
    pending: Vec<(u32, u32)>,
    /// Processes already walked, by pid; a pid is entered only once its
    /// main thread passed its check.
    processes: HashSet<u32>,
    tasks: HashSet<u32>,
}

impl Walk {
    /// Walk one process: its main thread first and explicitly, whether or
    /// not the listing names it, so no other thread of a process is sent
    /// before the process is checked. A child must still be its parent's
    /// by its own stat: the pid it was named by may have been reused before
    /// it was opened. An unread main thread or stat cannot show that, so the
    /// process is skipped whole.
    fn process(&mut self, dir: &ProcDir, parent: Option<u32>, send: &impl Fn(Msg) -> bool) -> Visit {
        let pid = dir.pid();
        if self.processes.contains(&pid) {
            return Visit::Done;
        }
        if self.count >= TASK_CAP {
            self.truncated = true;
            return Visit::Done;
        }
        let main = match dir.task(pid) {
            Ok(main) => main,
            Err(e) => {
                if !gone(&e) {
                    self.gaps += 1;
                }
                return if parent.is_none() && gone(&e) {
                    Visit::RootGone
                } else {
                    Visit::Done
                };
            }
        };
        let mut row = read_task(&main, pid, pid, self.requested, &mut self.gaps);
        row.parent = parent;
        if let Some(parent) = parent
            && row.ppid != Some(parent)
        {
            if row.ppid.is_none() {
                self.gaps += 1;
            }
            return Visit::Done;
        }
        self.processes.insert(pid);
        if !self.emit(&main, row, send) {
            return Visit::Closed;
        }
        let tids = match dir.tids() {
            Ok((tids, listing_gaps)) => {
                self.gaps += listing_gaps;
                tids
            }
            Err(e) => {
                if !gone(&e) {
                    self.gaps += 1;
                }
                return Visit::Done;
            }
        };
        for tid in tids.into_iter().filter(|tid| *tid != pid) {
            if self.count >= TASK_CAP {
                self.truncated = true;
                return Visit::Done;
            }
            let task = match dir.task(tid) {
                Ok(task) => task,
                // Exited since the listing: nothing to read, not a gap.
                Err(e) if gone(&e) => continue,
                Err(_) => {
                    self.gaps += 1;
                    continue;
                }
            };
            let mut row = read_task(&task, pid, tid, self.requested, &mut self.gaps);
            row.parent = parent;
            if !self.emit(&task, row, send) {
                return Visit::Closed;
            }
        }
        Visit::Done
    }

    /// Send one row and queue the children its task names. False when the
    /// pass has closed.
    fn emit(&mut self, task: &TaskDir, row: TaskRow, send: &impl Fn(Msg) -> bool) -> bool {
        if !self.tasks.insert(row.tid) {
            return true;
        }
        self.count += 1;
        let pid = row.pid;
        if !send(Msg::Row(Box::new(row))) {
            return false;
        }
        match task.read("children") {
            Ok(bytes) => {
                for child in String::from_utf8_lossy(&bytes)
                    .split_whitespace()
                    .filter_map(|p| p.parse().ok())
                {
                    // The queue has its own bound, the cap rows have too.
                    if self.pending.len() >= TASK_CAP {
                        self.truncated = true;
                        break;
                    }
                    self.pending.push((child, pid));
                }
            }
            Err(_) => self.gaps += 1,
        }
        true
    }
}

fn read_task(dir: &TaskDir, pid: u32, tid: u32, requested: Instant, gaps: &mut usize) -> TaskRow {
    let read_start = requested.elapsed();
    let mut row = TaskRow {
        pid,
        tid,
        read_start,
        ..TaskRow::default()
    };
    // The wait channel first: it is the reading the stop destroys, and the
    // one most likely to change while the rest are read.
    row.wchan = dir.text("wchan", gaps).map(|w| w.trim().to_owned());
    match dir.read("stat") {
        Ok(stat) => parse_stat(&stat, &mut row),
        Err(_) => *gaps += 1,
    }
    row.syscall = dir.text("syscall", gaps).map(|s| s.trim().to_owned());
    if let Some(sched) = dir.text("schedstat", gaps) {
        row.sched_at = Some(requested.elapsed());
        let mut fields = sched.split_whitespace().map(|f| f.parse::<u64>().ok());
        row.on_cpu_ns = fields.next().flatten();
        row.runqueue_ns = fields.next().flatten();
    }
    if let Some(status) = dir.text("status", gaps) {
        row.status_at = Some(requested.elapsed());
        for line in status.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key {
                "voluntary_ctxt_switches" => row.voluntary = value.parse().ok(),
                "nonvoluntary_ctxt_switches" => row.involuntary = value.parse().ok(),
                "Cpus_allowed_list" => row.cpus_allowed = Some(value.to_owned()),
                _ => {}
            }
        }
    }
    row.read_end = requested.elapsed();
    row
}

/// Fill comm, state, ppid, pgid and start time from a `stat` record. The
/// comm is the bytes between the first `(` and the last `)`, since it may
/// hold either, and need not be UTF-8.
fn parse_stat(stat: &[u8], row: &mut TaskRow) {
    let (Some(open), Some(close)) = (
        stat.iter().position(|b| *b == b'('),
        stat.iter().rposition(|b| *b == b')'),
    ) else {
        return;
    };
    if close <= open {
        return;
    }
    row.comm = Some(stat[open + 1..close].to_vec());
    let rest = String::from_utf8_lossy(&stat[close + 1..]);
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // Fields after the comm, 0-based: state is field 3 of proc_pid_stat(5),
    // ppid 4, pgrp 5, starttime 22.
    row.state = fields.first().and_then(|s| s.chars().next());
    row.ppid = fields.get(1).and_then(|s| s.parse().ok());
    row.pgid = fields.get(2).and_then(|s| s.parse().ok());
    row.start = fields.get(19).and_then(|s| s.parse().ok());
}

fn read_host(requested: Instant) -> HostRow {
    let read = |path: &str| fs::read_to_string(path).ok();
    let mut host = HostRow {
        loadavg: read("/proc/loadavg").map(|s| s.trim().to_owned()),
        ..HostRow::default()
    };
    for resource in ["cpu", "io", "memory"] {
        let Some(text) = read(&format!("/proc/pressure/{resource}")) else {
            continue;
        };
        let at = requested.elapsed();
        let total = text
            .lines()
            .find(|l| l.starts_with("some "))
            .and_then(|l| l.split_whitespace().find_map(|f| f.strip_prefix("total=")))
            .and_then(|t| t.parse().ok());
        if let Some(total) = total {
            host.pressure.push((resource, total, at));
        }
    }
    host
}

/// The baseline a watchdog takes when a deadline comes within
/// [`PRESAMPLE_LEAD`]: at most one outstanding, at most one admitted per lead.
/// Its rows are drained without blocking on every watchdog tick, so nothing
/// here ever waits. A presample still reading after twice the lead is
/// abandoned, so one stuck collector cannot end this watchdog's baselines.
#[derive(Default)]
pub(super) struct Presampler {
    admitted: Option<Instant>,
    pending: Option<(Receiver<Msg>, PassRecord)>,
    /// Finished baselines, newest last; two are kept so a newer one that
    /// turns out to overlap the live pass leaves the older one usable.
    finished: Vec<PassRecord>,
}

impl Presampler {
    /// Called on every watchdog tick with the root and the time to the
    /// nearest deadline.
    pub(super) fn tick(&mut self, root: Option<&Arc<ProcDir>>, nearest: Option<Duration>) {
        self.drain();
        if let Some((_, record)) = &self.pending
            && record.requested.elapsed() >= 2 * PRESAMPLE_LEAD
        {
            self.pending = None;
        }
        let due = nearest.is_some_and(|d| d <= PRESAMPLE_LEAD);
        let throttled = self
            .admitted
            .is_some_and(|at| at.elapsed() < PRESAMPLE_LEAD);
        let Some(root) = root else {
            return;
        };
        if !due || throttled || self.pending.is_some() {
            return;
        }
        let now = Instant::now();
        self.admitted = Some(now);
        let (requested, rx) = request(Arc::clone(root), now + 2 * PRESAMPLE_LEAD);
        self.pending = Some((
            rx,
            PassRecord::empty(Pass::Presample, requested, Coverage::CutOff),
        ));
    }

    fn drain(&mut self) {
        let Some((rx, record)) = self.pending.as_mut() else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    if absorb(record, msg) {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => return,
                // A presample that ended without its terminal record is not
                // a baseline.
                Err(TryRecvError::Disconnected) => {
                    self.pending = None;
                    return;
                }
            }
        }
        if let Some((_, record)) = self.pending.take()
            && record.coverage.finished()
            && !record.rows.is_empty()
        {
            self.finished.push(record);
            if self.finished.len() > 2 {
                self.finished.remove(0);
            }
        }
    }

    /// The newest finished baseline whose walk, by its terminal record's
    /// stamp, ended before `before`.
    pub(super) fn baseline(mut self, before: Instant) -> Option<PassRecord> {
        self.drain();
        self.finished
            .into_iter()
            .rev()
            .find(|record| record.ended.is_some_and(|ended| ended < before))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn stat_parsing_survives_a_comm_with_parens_spaces_and_bad_utf8() {
        let mut row = TaskRow::default();
        let mut stat = b"42 (a (b) \xe2\x82".to_vec();
        stat.extend_from_slice(b") S 7 42 42 0 -1 4194560 0 0 0 0 7 3 0 0 20 0 1 0 12345 0 0");
        parse_stat(&stat, &mut row);
        assert_eq!(row.comm.as_deref(), Some(&b"a (b) \xe2\x82"[..]));
        assert_eq!(row.state, Some('S'));
        assert_eq!(row.ppid, Some(7));
        assert_eq!(row.pgid, Some(42));
        assert_eq!(row.start, Some(12345));
        assert!(!row.comm_text().contains('\t'));
    }

    fn own_root() -> Arc<ProcDir> {
        Arc::new(ProcDir::open(std::process::id()).unwrap())
    }

    #[test]
    fn a_pass_over_this_process_reads_its_threads() {
        ensure_dispatcher();
        let record = run_pass(
            Some(&own_root()),
            Pass::Live,
            Instant::now() + Duration::from_secs(10),
        );
        assert!(
            matches!(
                record.coverage,
                Coverage::Complete | Coverage::Partial { .. } | Coverage::Truncated { .. }
            ),
            "{:?}",
            record.coverage
        );
        // SAFETY: gettid takes no arguments and cannot fail.
        let me = u32::try_from(unsafe { libc::gettid() }).unwrap();
        let row = record.rows.iter().find(|r| r.tid == me).expect("own thread");
        assert!(row.comm.is_some() && row.state.is_some() && row.start.is_some());
        assert!(row.on_cpu_ns.is_some() && row.sched_at.is_some() && row.status_at.is_some());
    }

    /// A pinned directory never reaches a later process under the same pid:
    /// once its process is reaped, a pass over it reads nothing.
    #[test]
    fn a_reaped_root_is_not_read() {
        ensure_dispatcher();
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let root = Arc::new(ProcDir::open(child.id()).unwrap());
        child.kill().unwrap();
        child.wait().unwrap();
        let record = run_pass(
            Some(&root),
            Pass::Live,
            Instant::now() + Duration::from_secs(10),
        );
        assert_eq!(record.coverage, Coverage::RootGone);
        assert!(record.rows.is_empty());
    }

    #[test]
    fn a_pass_without_a_root_reads_nothing() {
        let record = run_pass(None, Pass::Live, Instant::now() + Duration::from_secs(10));
        assert_eq!(record.coverage, Coverage::NoRoot);
    }

    #[test]
    fn a_pass_past_its_cutoff_returns_at_once() {
        ensure_dispatcher();
        let started = Instant::now();
        let record = run_pass(Some(&own_root()), Pass::Live, started);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(record.rows.is_empty());
        assert_eq!(record.coverage, Coverage::Unavailable);
    }
}
