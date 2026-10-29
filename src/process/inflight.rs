//! What a process subtree is doing right now, for a report written just before
//! it is killed: every live descendant with its age and command line, and for
//! one blocked on a file lock, the lock's path and the process holding it.
//!
//! A time ceiling names the phase whose clock ran out, which is not always
//! what the run was waiting on: a background script check runs beside the
//! phases, and a cargo blocked on another cargo's lock (`Blocking waiting for
//! file lock`, printed into a captured stream nobody sees) looks from outside
//! like a phase that simply never finished. The kernel knows who waits on whom
//! (`/proc/locks` lists a blocked request as `->` beside the granted lock it
//! waits behind), so the report says it instead of leaving it to inference.

use std::collections::HashMap;

use super::{Pid, ProcStat, ProcTable};

/// One process of the subtree, in tree order.
#[derive(Debug)]
pub(crate) struct InFlight {
    /// Distance from the root: 0 for the root's own children.
    pub depth: usize,
    pub pid: u32,
    /// Seconds since the process started, when computable.
    pub age_secs: Option<u64>,
    /// The command line, or `[comm]` when it cannot be read.
    pub command: String,
    pub waits_on: Option<LockWait>,
}

/// A file lock a process is blocked on.
#[derive(Debug)]
pub(crate) struct LockWait {
    /// The locked file, when one of the waiter's descriptors names it.
    pub path: Option<String>,
    /// The process recorded on the granted lock, and its command line.
    pub holder: Option<(u32, String)>,
}

/// The live descendants of `root`, parents before their children and
/// siblings oldest first, and whether `root` itself is blocked on a lock.
pub(crate) fn subtree(root: u32) -> (Vec<InFlight>, Option<LockWait>) {
    let Ok(table) = ProcTable::snapshot() else {
        return (Vec::new(), None);
    };
    let locks = std::fs::read_to_string("/proc/locks")
        .map(|text| parse_locks(&text))
        .unwrap_or_default();
    let uptime = uptime_secs();
    let hz = super::clock_ticks_per_second();

    let mut children: HashMap<u32, Vec<&ProcStat>> = HashMap::new();
    for (_, stat) in table.iter() {
        children
            .entry(stat.parent().map_or(0, Pid::get))
            .or_default()
            .push(stat);
    }
    for kids in children.values_mut() {
        kids.sort_by_key(|stat| (stat.start(), stat.identity().pid().get()));
    }

    let mut out = Vec::new();
    let mut stack: Vec<(usize, &ProcStat)> = children
        .get(&root)
        .map(|kids| kids.iter().rev().map(|stat| (0, *stat)).collect())
        .unwrap_or_default();
    // Bounded like `shutdown::descendants`: an inconsistent snapshot (a pid
    // reused mid-scan) could hold a cycle.
    while let Some((depth, stat)) = stack.pop() {
        if out.len() > 65_536 {
            break;
        }
        let pid = stat.identity().pid().get();
        let age_secs = match (uptime, hz) {
            (Some(up), Some(hz)) => Some(up.saturating_sub(stat.start().get() / hz)),
            _ => None,
        };
        out.push(InFlight {
            depth,
            pid,
            age_secs,
            command: command_line(pid).unwrap_or_else(|| format!("[{}]", stat.comm())),
            waits_on: lock_wait(pid, &locks),
        });
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().rev().map(|kid| (depth + 1, *kid)));
        }
    }
    (out, lock_wait(root, &locks))
}

/// One row of `/proc/locks`.
#[derive(Debug, PartialEq, Eq)]
struct LockRow {
    /// The row's lock id: a blocked request shares it with the granted lock it
    /// waits behind.
    id: u64,
    /// A `->` row: a request blocked behind the lock with the same `id`.
    blocked: bool,
    pid: u32,
    /// `major:minor` in hex as procfs prints them, then the inode.
    dev: (u32, u32),
    inode: u64,
}

/// Parse `/proc/locks`. Rows look like
/// `3: FLOCK  ADVISORY  WRITE 4242 08:02:1234 0 EOF`, and a blocked request
/// like `3: -> FLOCK  ADVISORY  WRITE 4243 08:02:1234 0 EOF`. A row that does
/// not parse is skipped: the report is best-effort, never a failure.
fn parse_locks(text: &str) -> Vec<LockRow> {
    text.lines()
        .filter_map(|line| {
            let (id, rest) = line.split_once(':')?;
            let id = id.trim().parse().ok()?;
            let mut fields = rest.split_whitespace().peekable();
            let blocked = fields.next_if_eq(&"->").is_some();
            // kind, advisory/mandatory, access mode, then the pid.
            let pid = fields.nth(3)?.parse().ok()?;
            let mut location = fields.next()?.split(':');
            let major = u32::from_str_radix(location.next()?, 16).ok()?;
            let minor = u32::from_str_radix(location.next()?, 16).ok()?;
            let inode = location.next()?.parse().ok()?;
            Some(LockRow {
                id,
                blocked,
                pid,
                dev: (major, minor),
                inode,
            })
        })
        .collect()
}

/// The lock `pid` is blocked on, if `/proc/locks` lists a request of its as
/// waiting.
fn lock_wait(pid: u32, locks: &[LockRow]) -> Option<LockWait> {
    let request = locks.iter().find(|row| row.blocked && row.pid == pid)?;
    let holder = locks
        .iter()
        .find(|row| !row.blocked && row.id == request.id)
        .map(|row| {
            (
                row.pid,
                command_line(row.pid).unwrap_or_else(|| "?".to_owned()),
            )
        });
    Some(LockWait {
        path: locked_path(pid, request.dev, request.inode),
        holder,
    })
}

/// The path of the descriptor of `pid` open on the file `dev`/`inode`.
fn locked_path(pid: u32, dev: (u32, u32), inode: u64) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    for fd in fds.flatten() {
        let Ok(meta) = std::fs::metadata(fd.path()) else {
            continue;
        };
        if meta.ino() == inode && dev_numbers(meta.dev()) == dev {
            return std::fs::read_link(fd.path())
                .ok()
                .map(|p| p.display().to_string());
        }
    }
    None
}

/// Linux's `dev_t` split into major and minor, as `MAJOR()`/`MINOR()` do.
// Each half is masked to its bit width before the cast, so nothing is lost.
#[allow(clippy::cast_possible_truncation)]
const fn dev_numbers(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major as u32, minor as u32)
}

/// `pid`'s command line, NULs as spaces, credentials redacted, clipped.
fn command_line(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let text: String = String::from_utf8_lossy(&raw)
        .split('\0')
        .filter(|arg| !arg.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return None;
    }
    let text = crate::history::redact_secret_text(&text);
    Some(crate::lockfile::observe::clip(&text))
}

fn uptime_secs() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    let secs = text.split_whitespace().next()?.split('.').next()?;
    secs.parse().ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn locks_parse_granted_and_blocked_rows() {
        let text = "1: POSIX  ADVISORY  WRITE 900 00:1f:77 0 EOF\n\
                    3: FLOCK  ADVISORY  WRITE 4242 08:02:1234 0 EOF\n\
                    3: -> FLOCK  ADVISORY  WRITE 4243 08:02:1234 0 EOF\n\
                    garbage\n";
        let rows = parse_locks(text);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(
            rows[2],
            LockRow {
                id: 3,
                blocked: true,
                pid: 4243,
                dev: (8, 2),
                inode: 1234
            }
        );
        assert!(!rows[1].blocked);
        assert_eq!(rows[0].dev, (0, 0x1f));
    }

    #[test]
    fn a_blocked_request_names_the_granted_lock_with_its_id() {
        let rows = parse_locks(
            "3: FLOCK  ADVISORY  WRITE 4242 08:02:1234 0 EOF\n\
             3: -> FLOCK  ADVISORY  WRITE 4243 08:02:1234 0 EOF\n",
        );
        let wait = lock_wait(4243, &rows).unwrap();
        assert_eq!(wait.holder.map(|(pid, _)| pid), Some(4242));
        assert!(
            lock_wait(4242, &rows).is_none(),
            "the holder waits on nothing"
        );
    }

    #[test]
    fn dev_numbers_match_procfs_rendering() {
        // makedev(8, 2) and a large minor in the extended encoding.
        assert_eq!(dev_numbers((8 << 8) | 2), (8, 2));
        assert_eq!(
            dev_numbers((259 << 8) | (0x12300 << 12) | 0x45),
            (259, 0x12345)
        );
    }

    /// A real flock wait between two processes of this test is reported, with
    /// the path and the holder.
    #[test]
    fn a_child_blocked_on_a_flock_is_reported_with_path_and_holder() {
        use std::os::fd::AsRawFd;
        let dir = crate::test_scratch::scratch("inflight", "flock_wait");
        let path = dir.join("held.lock");
        let file = std::fs::File::create(&path).unwrap();
        // SAFETY: an owned open descriptor.
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
        let mut child = std::process::Command::new("flock")
            .arg(&path)
            .arg("true")
            .spawn()
            .unwrap();
        let me = std::process::id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let row = loop {
            let (rows, _) = subtree(me);
            if let Some(row) = rows
                .into_iter()
                .find(|r| r.pid == child.id() && r.waits_on.is_some())
            {
                break row;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the child never showed as blocked"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        drop(file);
        child.wait().unwrap();
        let wait = row.waits_on.unwrap();
        // The kernel names the lock by its resolved path, so a scratch dir
        // reached through a symlinked target dir must be compared resolved.
        let resolved = std::fs::canonicalize(&path).unwrap();
        assert_eq!(
            wait.path.as_deref(),
            Some(resolved.display().to_string().as_str())
        );
        assert_eq!(wait.holder.map(|(pid, _)| pid), Some(me));
        assert!(row.command.contains("flock"), "{}", row.command);
    }
}
