//! Hang attribution, the evidence record, and user-facing timeout reports.
//!
//! Everything here runs after the kill: the evidence it renders was collected
//! by `evidence`'s collectors while the deadline owner waited at most its
//! allowances, and writing the snapshot directory touches no target.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::evidence::{PassRecord, Root, TaskRow};
use super::leader::SignalOutcome;
use super::{Attribution, HungTest, Leader, TimeoutReason};

/// A process whose threads may be the charged test's, with the test names
/// its own stream had in flight. Only a process brokkr verified (a direct
/// test binary, or a shim peer by its admitted identity) is a candidate, and
/// its threads are named only where a reading's process start matches.
#[derive(Clone, Debug)]
pub(super) struct Candidate {
    pub(super) root: Root,
    pub(super) names: Vec<String>,
}

/// What the deadline owner gathered, in the order it happened.
pub(super) struct Gathered {
    pub(super) verdict_at: Instant,
    pub(super) baseline: Option<PassRecord>,
    pub(super) live: PassRecord,
    pub(super) stop: SignalOutcome,
    pub(super) stop_at: Duration,
    pub(super) post: PassRecord,
    pub(super) kill: SignalOutcome,
    pub(super) kill_at: Duration,
    pub(super) unreaped_leaders: usize,
    pub(super) live_harnesses: Option<usize>,
    pub(super) candidates: Vec<Candidate>,
}

/// The verdict a record is filed under.
pub(super) struct Filed {
    pub(super) leader: Leader,
    pub(super) leader_pid: u32,
    pub(super) reason: TimeoutReason,
    pub(super) attribution: Attribution,
    pub(super) elapsed: Duration,
    pub(super) ceiling: Duration,
}

pub(super) fn record_hung_test(state_root: &Path, filed: Filed, gathered: &Gathered) -> HungTest {
    let component = filed
        .attribution
        .test_name()
        .map(str::to_owned)
        .unwrap_or_else(|| match &filed.reason {
            TimeoutReason::Wall { .. } => "wall".to_owned(),
            TimeoutReason::NoCompletion => "no-completion".to_owned(),
            TimeoutReason::Idle => "idle".to_owned(),
            TimeoutReason::Watched => "watched".to_owned(),
            TimeoutReason::OneTestWall => "one-test-wall".to_owned(),
        });
    // Direct children seen in the leader's group: the processes the group
    // signals were sent to, beside the leader. Whether each one exited is not
    // observed here.
    let mut test_pids: Vec<u32> = [&gathered.live, &gathered.post]
        .iter()
        .flat_map(|pass| pass.rows.iter())
        .filter(|row| {
            row.parent == Some(filed.leader_pid)
                && row.pid == row.tid
                && row.pgid == Some(filed.leader_pid)
        })
        .map(|row| row.pid)
        .collect();
    test_pids.sort_unstable();
    test_pids.dedup();
    let evidence = evidence_lines(filed.leader_pid, gathered);

    // Diagnostic snapshots are brokkr-owned state, so they live under the
    // config-dir `project_root` (`state_root`), not the code tree cargo runs
    // in - the two differ when brokkr.toml is one level above a foreign
    // checkout, and the foreign repo must stay clean.
    static SNAPSHOTS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _snapshots = SNAPSHOTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let snapshots = crate::clean::layout::ProjectState::new(state_root).test_hung_dir();
    prune_snapshots(&snapshots);
    let snapshot_dir = snapshots.join(format!(
        "{}-{}-{}",
        unix_secs(),
        filed.leader_pid,
        sanitize_path_component(&component)
    ));
    let snapshot_error = fs::create_dir_all(&snapshot_dir)
        .and_then(|()| {
            fs::write(
                snapshot_dir.join("evidence.txt"),
                evidence_file(&evidence, gathered),
            )
        })
        .err()
        .map(|e| e.to_string());

    HungTest {
        reason: filed.reason,
        attribution: filed.attribution,
        leader: filed.leader,
        elapsed: filed.elapsed,
        ceiling: filed.ceiling,
        snapshot_dir,
        leader_pid: filed.leader_pid,
        test_pids,
        killed: gathered.kill == SignalOutcome::Sent,
        evidence,
        snapshot_error,
    }
}

/// The first 15 bytes of a thread name: what Linux keeps of it in `comm`.
fn truncated_comm(name: &str) -> &[u8] {
    &name.as_bytes()[..name.len().min(15)]
}

fn ms(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}

fn secs_ns(ns: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // Display only.
    let s = ns as f64 / 1e9;
    format!("{s:.2}s")
}

/// A syscall reading as its number, with the name that number has in this
/// host's native table where it is a known one. The file does not say which
/// table the task entered through (a 64-bit image can still make a 32-bit
/// call), so the name is the table's, not a claim about the task.
pub(super) fn syscall_label(raw: &str) -> String {
    let first = raw.split_whitespace().next().unwrap_or(raw);
    match first.parse::<libc::c_long>() {
        Ok(nr) if nr >= 0 => match syscall_name(nr) {
            Some(name) => format!("#{nr} ({name} in the native table)"),
            None => format!("#{nr}"),
        },
        Ok(_) => "none (not in a syscall)".to_owned(),
        Err(_) => first.to_owned(),
    }
}

#[cfg(target_arch = "x86_64")]
fn syscall_name(nr: libc::c_long) -> Option<&'static str> {
    const KNOWN: &[(libc::c_long, &str)] = &[
        (libc::SYS_read, "read"),
        (libc::SYS_write, "write"),
        (libc::SYS_poll, "poll"),
        (libc::SYS_ppoll, "ppoll"),
        (libc::SYS_select, "select"),
        (libc::SYS_pselect6, "pselect6"),
        (libc::SYS_epoll_wait, "epoll_wait"),
        (libc::SYS_epoll_pwait, "epoll_pwait"),
        (libc::SYS_futex, "futex"),
        (libc::SYS_futex_waitv, "futex_waitv"),
        (libc::SYS_nanosleep, "nanosleep"),
        (libc::SYS_clock_nanosleep, "clock_nanosleep"),
        (libc::SYS_wait4, "wait4"),
        (libc::SYS_waitid, "waitid"),
        (libc::SYS_accept, "accept"),
        (libc::SYS_accept4, "accept4"),
        (libc::SYS_connect, "connect"),
        (libc::SYS_recvfrom, "recvfrom"),
        (libc::SYS_recvmsg, "recvmsg"),
        (libc::SYS_sendto, "sendto"),
        (libc::SYS_flock, "flock"),
        (libc::SYS_fcntl, "fcntl"),
        (libc::SYS_fsync, "fsync"),
        (libc::SYS_fdatasync, "fdatasync"),
        (libc::SYS_openat, "openat"),
        (libc::SYS_io_uring_enter, "io_uring_enter"),
        (libc::SYS_sched_yield, "sched_yield"),
        (libc::SYS_pause, "pause"),
        (libc::SYS_rt_sigsuspend, "rt_sigsuspend"),
        (libc::SYS_rt_sigtimedwait, "rt_sigtimedwait"),
    ];
    KNOWN.iter().find(|(n, _)| *n == nr).map(|(_, name)| *name)
}

#[cfg(not(target_arch = "x86_64"))]
fn syscall_name(_nr: libc::c_long) -> Option<&'static str> {
    None
}

fn state_label(state: Option<char>) -> String {
    match state {
        Some('R') => "R (running or runnable)".into(),
        Some('S') => "S (sleeping)".into(),
        Some('D') => "D (uninterruptible)".into(),
        Some('T') => "T (stopped)".into(),
        Some('t') => "t (traced)".into(),
        Some('Z') => "Z (zombie)".into(),
        Some(c) => c.to_string(),
        None => "unread".into(),
    }
}

fn wchan_label(wchan: Option<&str>) -> String {
    match wchan {
        Some("0") | Some("") => "no wait channel".into(),
        Some(w) => format!("waiting in {w}"),
        None => "wait channel unread".into(),
    }
}

/// Counter growth from `before` to `after`, absent when either is unread or
/// the reads overlap or run backwards.
fn delta(before: Option<u64>, after: Option<u64>) -> Option<u64> {
    after?.checked_sub(before?)
}

/// One candidate thread's line: what the live pass saw, what the
/// post-stop-request pass saw, the baseline window and the lifetime totals.
fn thread_line(row: &TaskRow, gathered: &Gathered) -> String {
    let mut line = format!(
        "candidate thread {} (tid {}, pid {}): live {}, {}",
        row.comm_text(),
        row.tid,
        row.pid,
        state_label(row.state),
        wchan_label(row.wchan.as_deref()),
    );
    if let Some(sys) = &row.syscall {
        line.push_str(&format!(", syscall {}", syscall_label(sys)));
    }
    if let Some(after) = gathered.post.matching(row) {
        // Each is the reading of its own file at its own moment: the state
        // and the syscall need not describe the same instant.
        line.push_str(&format!(
            "; after the stop request {}",
            state_label(after.state)
        ));
        if let Some(sys) = &after.syscall {
            line.push_str(&format!(", syscall {}", syscall_label(sys)));
        }
    } else {
        line.push_str("; not seen after the stop request");
    }
    // The window the counters describe: between the two schedstat reads.
    // Each counter pair is labelled with the interval between its own two
    // reads: schedstat for CPU and runqueue time, status for switches.
    let interval = |base: &PassRecord, before: Option<Duration>, after: Option<Duration>| {
        (gathered.live.requested + after?).checked_duration_since(base.requested + before?)
    };
    if let Some(base) = gathered.baseline.as_ref()
        && let Some(before) = base.matching(row)
    {
        if let Some(span) = interval(base, before.sched_at, row.sched_at) {
            line.push_str(&format!(
                "; between schedstat reads {:.1}s apart:",
                span.as_secs_f64()
            ));
            if let Some(cpu) = delta(before.on_cpu_ns, row.on_cpu_ns) {
                line.push_str(&format!(" on CPU {}", secs_ns(cpu)));
            }
            if let Some(wait) = delta(before.runqueue_ns, row.runqueue_ns) {
                line.push_str(&format!(", waiting for a CPU {}", secs_ns(wait)));
            }
        }
        if let Some(span) = interval(base, before.status_at, row.status_at)
            && let Some((v, n)) = delta(before.voluntary, row.voluntary)
                .zip(delta(before.involuntary, row.involuntary))
        {
            line.push_str(&format!(
                "; {v} voluntary and {n} involuntary switches between status reads {:.1}s apart",
                span.as_secs_f64()
            ));
        }
        if before.wchan.is_some() && before.wchan == row.wchan {
            line.push_str("; same wait channel in the baseline and live reads");
        }
    }
    if let (Some(cpu), Some(wait)) = (row.on_cpu_ns, row.runqueue_ns) {
        line.push_str(&format!(
            "; thread lifetime on CPU {}, waiting for a CPU {}",
            secs_ns(cpu),
            secs_ns(wait)
        ));
    }
    if let Some(cpus) = &row.cpus_allowed {
        line.push_str(&format!("; may run on CPUs {cpus}"));
    }
    line
}

/// The host line: load at the verdict, brokkr's own concurrency, and PSI
/// growth between the baseline's host read and the live one.
fn host_line(gathered: &Gathered) -> String {
    let mut host = format!(
        "at the verdict {} supervised test group leader(s) not yet reaped",
        gathered.unreaped_leaders
    );
    if let Some(n) = gathered.live_harnesses {
        host.push_str(&format!(", {n} live harness(es) in this session"));
    }
    if let Some(load) = gathered.live.host.as_ref().and_then(|h| h.loadavg.as_ref()) {
        host.push_str(&format!("; loadavg in the live pass {load}"));
    }
    let reads = gathered
        .baseline
        .as_ref()
        .and_then(|b| b.host.as_ref().map(|h| (b, h)))
        .zip(gathered.live.host.as_ref());
    if let Some(((base, before), now)) = reads {
        for (resource, total, at) in &now.pressure {
            // Each counter's own two reads bound its interval.
            if let Some((_, was, was_at)) = before.pressure.iter().find(|(r, ..)| r == resource)
                && let Some(grew) = total.checked_sub(*was)
                && let Some(span) = (gathered.live.requested + *at)
                    .checked_duration_since(base.requested + *was_at)
            {
                host.push_str(&format!(
                    "; {resource} pressure (some) {} ms between reads {:.1}s apart",
                    grew / 1000,
                    span.as_secs_f64()
                ));
            }
        }
    }
    host
}

/// The report's evidence lines, observations only: no line says why the test
/// hung, since no counter here can.
fn evidence_lines(leader_pid: u32, gathered: &Gathered) -> Vec<String> {
    const SHOWN: usize = 8;
    let mut lines = Vec::new();
    for candidate in &gathered.candidates {
        let pid = candidate.root.pid;
        let wanted: Vec<&[u8]> = candidate
            .names
            .iter()
            .map(|n| truncated_comm(n))
            .collect();
        // The process the live pass read under this pid must be the one
        // brokkr verified, by start time.
        let same_process = candidate.root.start.is_none()
            || gathered.live.process_start(pid) == candidate.root.start;
        let threads: Vec<&TaskRow> = gathered
            .live
            .rows
            .iter()
            .filter(|row| same_process && row.pid == pid)
            .filter(|row| row.comm.as_deref().is_some_and(|c| wanted.contains(&c)))
            .collect();
        if threads.is_empty() {
            let names = if candidate.names.is_empty() {
                "no test name known".to_owned()
            } else {
                candidate.names.join(", ")
            };
            lines.push(if gathered.live.coverage.whole() && same_process {
                format!("no live thread of pid {pid} is named for {names}")
            } else if gathered.live.coverage.whole() {
                format!("pid {pid} was no longer the verified harness when the live pass read it")
            } else {
                format!(
                    "no thread of pid {pid} named for {names} among the tasks read (live pass {})",
                    gathered.live.coverage.describe()
                )
            });
        }
        for row in threads.iter().take(SHOWN) {
            lines.push(thread_line(row, gathered));
        }
        if threads.len() > SHOWN {
            lines.push(format!(
                "{} more threads of pid {pid} match those names",
                threads.len() - SHOWN,
            ));
        }
    }

    lines.push(host_line(gathered));

    let in_group: Vec<&TaskRow> = gathered
        .post
        .rows
        .iter()
        .filter(|row| row.pgid == Some(leader_pid))
        .collect();
    let stopped = in_group.iter().filter(|row| row.state == Some('T')).count();
    // A baseline is dated by when its walk ended, which may be just after
    // the verdict: it only has to end before the live pass began.
    let baseline = match &gathered.baseline {
        Some(base) => {
            let ended = base.ended.unwrap_or(base.requested);
            let (gap, side) = if ended <= gathered.verdict_at {
                (gathered.verdict_at - ended, "before")
            } else {
                (ended - gathered.verdict_at, "after")
            };
            format!(
                "baseline read ending {} {side} the verdict, {}",
                ms(gap),
                base.coverage.describe()
            )
        }
        None => "no baseline".to_owned(),
    };
    lines.push(format!(
        "stop {} at +{}, kill {} at +{} after the verdict; live pass {}, post-stop-request pass {} ({stopped} of {} group tasks it read were in T when read); {baseline}",
        gathered.stop.describe(),
        ms(gathered.stop_at),
        gathered.kill.describe(),
        ms(gathered.kill_at),
        gathered.live.coverage.describe(),
        gathered.post.coverage.describe(),
        in_group.len(),
    ));
    lines
}

/// The snapshot file: the evidence lines, then every row of every pass.
fn evidence_file(lines: &[String], gathered: &Gathered) -> String {
    let mut out = String::new();
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    let passes = gathered
        .baseline
        .iter()
        .chain([&gathered.live, &gathered.post]);
    for pass in passes {
        let (offset, side) = if pass.requested >= gathered.verdict_at {
            (pass.requested - gathered.verdict_at, "after")
        } else {
            (gathered.verdict_at - pass.requested, "before")
        };
        out.push_str(&format!(
            "\n# {} pass, {}, requested {} {side} the verdict\n",
            pass.pass.label(),
            pass.coverage.describe(),
            ms(offset),
        ));
        if let Some(host) = &pass.host {
            out.push_str(&format!(
                "host: loadavg {}",
                host.loadavg.as_deref().unwrap_or("unread")
            ));
            for (resource, total, at) in &host.pressure {
                out.push_str(&format!(
                    "; {resource} pressure some total {total} us at +{}",
                    ms(*at)
                ));
            }
            out.push('\n');
        }
        // `other_pass` says whether the same task (tid and start time) was
        // read in the other expiry pass, so a row seen only once is visible.
        out.push_str(
            "pid\ttid\tparent\tpgid\tstart\tcomm\tstate\twchan\tsyscall\ton_cpu_ns\trunqueue_ns\tvoluntary\tinvoluntary\tcpus\tread_start_us\tread_end_us\tother_pass\n",
        );
        let other = match pass.pass {
            super::evidence::Pass::Live => Some(&gathered.post),
            super::evidence::Pass::PostStop => Some(&gathered.live),
            super::evidence::Pass::Presample => None,
        };
        let opt = |v: Option<u64>| v.map_or_else(|| "-".to_owned(), |v| v.to_string());
        for row in &pass.rows {
            let seen = match other {
                Some(other) if other.matching(row).is_some() => "seen",
                Some(_) => "not seen",
                None => "-",
            };
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{seen}\n",
                row.pid,
                row.tid,
                row.parent.map_or_else(|| "-".to_owned(), |p| p.to_string()),
                row.pgid.map_or_else(|| "-".to_owned(), |p| p.to_string()),
                opt(row.start),
                row.comm_text(),
                row.state.map_or_else(|| "-".to_owned(), |c| c.to_string()),
                row.wchan.as_deref().unwrap_or("-"),
                row.syscall.as_deref().unwrap_or("-"),
                opt(row.on_cpu_ns),
                opt(row.runqueue_ns),
                opt(row.voluntary),
                opt(row.involuntary),
                row.cpus_allowed.as_deref().unwrap_or("-"),
                row.read_start.as_micros(),
                row.read_end.as_micros(),
            ));
        }
    }
    out
}

/// Diagnostics are a bounded recent history, rather than an archive. Keep
/// room for the snapshot about to be written. Never follow directory symlinks.
fn prune_snapshots(root: &Path) {
    const RETAINED: usize = 16;
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut directories: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }
            Some((metadata.modified().unwrap_or(UNIX_EPOCH), entry.path()))
        })
        .collect();
    directories.sort_by(|a, b| b.cmp(a));
    for (_, path) in directories.into_iter().skip(RETAINED - 1) {
        drop(fs::remove_dir_all(path));
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub(super) fn sanitize_path_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(120));
    let mut prev_sep = false;
    for ch in name.chars() {
        let keep = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_');
        if keep {
            out.push(ch);
            prev_sep = false;
        } else if !prev_sep {
            out.push('_');
            prev_sep = true;
        }
        if out.len() >= 120 {
            break;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "unknown".into()
    } else {
        trimmed.to_owned()
    }
}

/// True for libtest's standalone status lines (`ok`, `FAILED`, `ignored`,
/// optionally followed by ` <X.Xs>` when `--report-time` is enabled).
/// Also used by `test_cmd`'s display condenser: under `--nocapture` the
/// verdict lands on its own line after the test's output, so the
/// full-line `test NAME ... FAILED` filter never sees it.
pub(crate) fn is_bare_status_line(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let Some(head) = parts.next() else {
        return false;
    };
    if !matches!(head, "ok" | "FAILED" | "ignored") {
        return false;
    }
    // Allow at most one trailing token, and only if it looks like a
    // libtest timing suffix: `<0.001s>`.
    match parts.next() {
        None => true,
        Some(tail) => parts.next().is_none() && tail.starts_with('<') && tail.ends_with("s>"),
    }
}

/// The line naming where the hang snapshot went, or why there is none.
fn snapshot_line(hung: &HungTest, cwd: &Path) -> String {
    match &hung.snapshot_error {
        Some(err) => format!("snapshot failed: {err}"),
        None => {
            let path = hung
                .snapshot_dir
                .strip_prefix(cwd)
                .unwrap_or(&hung.snapshot_dir);
            format!("full snapshot: {}", path.display())
        }
    }
}

fn idle_cause(leader: Leader) -> &'static str {
    match leader {
        Leader::Cargo => {
            "cargo produced no test progress before the first test or after the last; a build-directory lock held by another cargo can cause this"
        }
        Leader::TestBinary => {
            "the directly run test binary produced no test progress before the first test or after the last"
        }
    }
}

fn wall_context(blamed: &[String]) -> String {
    match blamed {
        [] => "no test was in flight".to_owned(),
        names => format!(
            "in flight when it expired (unverified): {}",
            names.join(", ")
        ),
    }
}

fn child_text(pids: &[u32]) -> String {
    match pids {
        [] => "no child process seen in it".to_owned(),
        [pid] => format!("child process seen in it: pid {pid}"),
        pids => format!(
            "child processes seen in it: pids {}",
            pids.iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

pub(crate) fn format_hung_test(hung: &HungTest, cwd: &Path) -> String {
    let child_text = child_text(&hung.test_pids);
    // The measured time and the limit are one number when the kill is on time
    // (the usual case), so say it once; both appear only when they differ.
    let (ran, limit) = (hung.elapsed.as_secs(), hung.ceiling.as_secs());
    let timing = |what: &str| {
        if ran == limit {
            format!("exceeded the {limit}s {what}")
        } else {
            format!("ran {ran}s, past the {limit}s {what}")
        }
    };
    let named = |name: &str| match &hung.attribution {
        Attribution::Exact { .. } => format!("test {name}"),
        Attribution::Suspect { stale: true, .. } => format!("test {name} (best guess)"),
        Attribution::Suspect { .. } => format!("test {name} (suspect)"),
        Attribution::None => "an unnamed test".to_owned(),
    };

    let headline = match (&hung.reason, &hung.attribution) {
        (TimeoutReason::Wall { blamed }, Attribution::None) => {
            // Active when a wall deadline expired is context, not cause.
            let context = wall_context(blamed);
            let run = if ran == limit {
                format!("the run exceeded its {limit}s wall deadline")
            } else {
                format!("the run ran {ran}s, past its {limit}s wall deadline")
            };
            if hung.killed {
                format!("{run} and was killed\n  {context}")
            } else {
                format!("{run}; the kill was not confirmed\n  {context}")
            }
        }
        (TimeoutReason::Idle, _) => {
            let cause = idle_cause(hung.leader);
            format!(
                "no test in flight for {ran}s ({}) - {cause}",
                if ran == limit {
                    "the idle ceiling".to_owned()
                } else {
                    format!("past the {limit}s idle ceiling")
                },
            )
        }
        (TimeoutReason::NoCompletion, Attribution::Exact { name }) => {
            if hung.leader == Leader::TestBinary {
                format!(
                    "the direct test binary running `{name}` {} with no completion record observed",
                    timing("process ceiling"),
                )
            } else {
                format!(
                    "{} {} with no completion record observed",
                    named(name),
                    timing("per-test timeout"),
                )
            }
        }
        (TimeoutReason::NoCompletion, _) => format!(
            "no test completed for {ran}s ({}) - the test in progress could not be named",
            if ran == limit {
                "the per-test ceiling".to_owned()
            } else {
                format!("past the {limit}s per-test ceiling")
            },
        ),
        (TimeoutReason::Watched | TimeoutReason::OneTestWall, Attribution::Exact { name }) => {
            if hung.leader == Leader::TestBinary {
                format!(
                    "the direct test binary running `{name}` {}",
                    timing("process ceiling")
                )
            } else {
                format!("{} {}", named(name), timing("per-test timeout"))
            }
        }
        (
            TimeoutReason::Watched | TimeoutReason::OneTestWall,
            Attribution::Suspect { name, .. },
        ) => format!("{} {}", named(name), timing("per-test timeout")),
        (TimeoutReason::Watched | TimeoutReason::OneTestWall, Attribution::None) => {
            format!("an unnamed test {}", timing("per-test timeout"))
        }
        (TimeoutReason::Wall { .. }, _) => "the run wall deadline expired".to_owned(),
    };
    let group = match hung.leader {
        Leader::Cargo => "cargo process group",
        Leader::TestBinary => "the direct test binary's process group",
    };
    // A sent signal is the kernel accepting it for the group, not an
    // observed exit; the children are those a reading saw in the group.
    let mut out = format!(
        "{headline}\n  {} {group} (pgid {}); {child_text}",
        if hung.killed {
            "SIGKILL sent to the"
        } else {
            "no confirmed SIGKILL to the"
        },
        hung.leader_pid,
    );
    for line in &hung.evidence {
        out.push_str(&format!("\n  {line}"));
    }
    out.push_str(&format!("\n  {}", snapshot_line(hung, cwd)));
    out
}

#[cfg(test)]
mod retention_tests {
    #![allow(clippy::unwrap_used)]
    #[test]
    fn snapshot_retention_leaves_room_for_the_next_diagnostic() {
        let root = crate::test_scratch::scratch("test_runner_hung", "retention");
        for n in 0..30 {
            std::fs::create_dir(root.join(format!("snapshot-{n}"))).unwrap();
        }
        let preserved = root.join("other-file");
        std::fs::write(&preserved, "untouched").unwrap();
        super::prune_snapshots(&root);
        assert_eq!(
            std::fs::read_dir(&root)
                .unwrap()
                .filter(|e| e.as_ref().unwrap().file_type().unwrap().is_dir())
                .count(),
            15
        );
        assert!(preserved.exists());
    }
}
