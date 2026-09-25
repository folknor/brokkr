//! Test processes orphaned by a brokkr that died without running any handler.
//!
//! The test runners spawn their child into its own process group, register the
//! group for SIGINT/SIGTERM (`shutdown::GroupReaper`), and arm a parent-death
//! SIGKILL on it (`shutdown::die_with_parent`). A brokkr SIGKILLed by anything
//! other than `brokkr kill --hard` - the OOM killer - runs no handler, and the
//! death signal reaches only the direct child. On the serial lanes that child
//! is cargo, so the test binary cargo runs (and a doctest under rustdoc, and
//! anything a test spawned) outlives it, reparented away from anything brokkr
//! would ever walk. The stray reaper does not see it either: it matches the
//! cargo family by name, and a test binary has the name of its crate.
//!
//! # The mark and the liveness proof
//!
//! Every process a test runner starts carries [`MARKER_ENV`]`=<token>`, where
//! the token is minted once per brokkr process (lazily, at its first test
//! spawn). The environment is inherited, so the mark reaches the test binary
//! under cargo, rustdoc's doctest binaries and whatever a test spawns without
//! brokkr having to predict their shape.
//!
//! Alongside the token, the process creates `~/.brokkr/test-runs/<token>.lock`
//! and holds an exclusive `flock` on it for the rest of its life. That flock is
//! the liveness proof: the kernel releases it when the process dies, however it
//! dies, and it answers the same way from every PID namespace that shares the
//! filesystem. A pid-plus-starttime identity would not: a sandboxed brokkr
//! records a pid that means something else on the host, and "that pid is not
//! this process" would read as "the owner is dead" and kill a live test run.
//! Every failure of the proof is on the safe side - an unreadable directory, a
//! missing file, a flock that cannot be taken all leave processes alone.
//!
//! # The reap
//!
//! Every fresh hold of the global lock runs [`reap_after_lock`], beside the
//! stray reap: each `.lock` file whose flock can be taken belongs to a dead
//! brokkr, and every process whose environment carries that token is SIGKILLed,
//! newest first (a child always starts after its parent, so newest-first is
//! leaves-first without walking the tree), each checked against the starttime
//! read with it. The file is removed only once a rescan finds no carrier left,
//! so a process forked between the scan and the signal is caught next time
//! rather than forgotten.
//!
//! An environment that cannot be read reads as unmarked, so a token whose only
//! carriers are unreadable reads as carrier-free and its file goes. That is a
//! deliberate miss, never a kill: a test process that made itself non-dumpable
//! (`PR_SET_DUMPABLE=0`, a setuid exec) escapes the reap. Keeping the file
//! while any unreadable process exists would keep every file forever instead,
//! since long-lived non-dumpable processes of the same user (`ssh-agent`) are
//! ordinary.
//!
//! A brokkr that exits normally leaves its file too (there is no exit hook to
//! remove it, and none is wanted): the next reap claims it, finds nothing in the
//! common case, and deletes it. A process a test leaked past a clean exit is an
//! orphaned test process as much as one left by a crash, and goes the same way.
//!
//! Exempt, whatever token they carry: this process, its ancestors and its
//! descendants. The mark is inherited, so a brokkr started from inside a test
//! carries the outer run's token in its own environment - and so does every
//! non-test child it starts. If the outer brokkr is the one that died, the reap
//! must not kill the brokkr running it, the orphaned test binary it is running
//! under, or its own cargo.

use std::collections::{HashMap, HashSet};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crate::output;

/// The environment variable carrying the owning brokkr's token.
pub const MARKER_ENV: &str = "BROKKR_TEST_RUN";

/// `~/.brokkr/<DIR_NAME>/`: one liveness file per brokkr that ran tests.
const DIR_NAME: &str = "test-runs";

/// A registered, flocked liveness file.
const LIVE_EXT: &str = "lock";

/// A liveness file between creation and its flock. Renamed to [`LIVE_EXT`]
/// once locked, so a reaper can never observe a live registration unlocked.
const PENDING_EXT: &str = "pending";

/// How old an unlocked `.pending` file must be before a reap deletes it. The
/// window it covers is a few syscalls long; anything older was abandoned by a
/// process that died mid-registration, and no process ever carried its token.
const PENDING_GRACE: Duration = Duration::from_secs(60);

/// Scan-and-kill rounds per reap. A parent forked a child after the first scan
/// is caught by the second; beyond that the file stays for the next reap.
const REAP_PASSES: usize = 3;

/// How long a pass waits for the processes it signalled to finish dying. A
/// SIGKILLed process is normally gone in milliseconds; one stuck in
/// uninterruptible sleep is not, and the reap is not worth blocking on it.
const SETTLE_BUDGET: Duration = Duration::from_secs(1);

/// This process's registration: the token its test children carry and the
/// open file whose flock proves it is alive. Never dropped - the flock must
/// last exactly as long as the process.
struct Registration {
    token: String,
    _flock: std::fs::File,
}

static REGISTRATION: OnceLock<Option<Registration>> = OnceLock::new();

fn runs_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".brokkr").join(DIR_NAME))
}

/// This process's token, registering it on first use. `None` when registration
/// failed (warned once) - the test runs anyway, without orphan coverage, which
/// is where every run stood before this module.
///
/// Unit tests never register: that would write under the real `~/.brokkr`. The
/// registration itself is tested against a scratch directory through
/// [`register_in`].
pub fn marker() -> Option<&'static str> {
    if cfg!(test) {
        return None;
    }
    REGISTRATION
        .get_or_init(|| {
            let result = runs_dir()
                .ok_or_else(|| std::io::Error::other("$HOME is not set"))
                .and_then(|dir| register_in(&dir));
            match result {
                Ok(reg) => Some(reg),
                Err(e) => {
                    output::warn(&format!(
                        "could not register this run for orphan cleanup ({e}); a test process \
                         left behind if brokkr is SIGKILLed will not be reaped"
                    ));
                    None
                }
            }
        })
        .as_ref()
        .map(|r| r.token.as_str())
}

/// Stamp this process's token onto a test child. Called from the test runners'
/// spawn choke point, after the caller's env so a sweep env cannot displace it.
pub fn stamp(cmd: &mut std::process::Command) {
    if let Some(token) = marker() {
        cmd.env(MARKER_ENV, token);
    }
}

/// Create and flock a liveness file under `dir`. The file is created as
/// `.pending`, locked, then renamed to `.lock`, so no reaper ever sees a live
/// registration's `.lock` without its flock.
fn register_in(dir: &Path) -> std::io::Result<Registration> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::create_dir_all(dir)?;
    let token = crate::hold::mint_nonce()
        .ok_or_else(|| std::io::Error::other("no entropy for a run token"))?;
    let pending = dir.join(format!("{token}.{PENDING_EXT}"));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&pending)?;
    if !try_flock(&file) {
        let err = std::io::Error::last_os_error();
        drop(std::fs::remove_file(&pending));
        return Err(err);
    }
    let live = dir.join(format!("{token}.{LIVE_EXT}"));
    if let Err(e) = std::fs::rename(&pending, &live) {
        drop(std::fs::remove_file(&pending));
        return Err(e);
    }
    Ok(Registration { token, _flock: file })
}

/// Non-blocking exclusive flock. `true` when taken.
fn try_flock(file: &std::fs::File) -> bool {
    // SAFETY: flock takes an fd we own for the duration of the call and no
    // pointers.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Whether `s` is a token [`register_in`] could have minted: 32 lowercase hex
/// characters. Anything else in the directory is not ours to judge.
fn is_token(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A registration whose owner is dead: its flock is now held by the reaper,
/// which keeps it until the file is removed or left for the next reap.
struct DeadRun {
    token: String,
    path: PathBuf,
    _flock: std::fs::File,
}

/// Claim every dead registration under `dir`. Abandoned `.pending` files past
/// their grace period are deleted on the way; nothing ever carried their token.
fn claim_dead(dir: &Path) -> Vec<DeadRun> {
    let mut dead = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return dead;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some((stem, ext)) = name.to_str().and_then(|n| n.split_once('.')) else {
            continue;
        };
        if !is_token(stem) {
            continue;
        }
        let path = entry.path();
        match ext {
            LIVE_EXT => {
                let Ok(file) = std::fs::File::open(&path) else { continue };
                if try_flock(&file) {
                    dead.push(DeadRun { token: stem.to_owned(), path, _flock: file });
                }
            }
            PENDING_EXT => {
                let old = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age >= PENDING_GRACE);
                if old
                    && let Ok(file) = std::fs::File::open(&path)
                    && try_flock(&file)
                {
                    drop(std::fs::remove_file(&path));
                }
            }
            _ => {}
        }
    }
    dead
}

/// One process as the reap sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcRow {
    ppid: u32,
    comm: String,
    /// `/proc/<pid>/stat` starttime, the identity token the SIGKILL is
    /// checked against.
    starttime: String,
    /// The [`MARKER_ENV`] value from `/proc/<pid>/environ`, when readable and
    /// present. Unreadable (another user's process, a kernel thread, a zombie)
    /// reads as unmarked.
    token: Option<String>,
}

/// A process selected for SIGKILL.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Orphan {
    pid: u32,
    comm: String,
    starttime: String,
    token: String,
}

/// The [`MARKER_ENV`] value in a raw `/proc/<pid>/environ` block, first
/// occurrence winning as it does for `getenv`.
fn marker_in_environ(environ: &[u8]) -> Option<String> {
    let prefix = format!("{MARKER_ENV}=");
    environ
        .split(|&b| b == 0)
        .find_map(|entry| entry.strip_prefix(prefix.as_bytes()))
        .and_then(|v| std::str::from_utf8(v).ok())
        .map(str::to_owned)
}

fn read_table() -> HashMap<u32, ProcRow> {
    let mut table = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return table;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // Same parsing as `stray::read_proc`: comm may contain spaces and
        // parentheses, so split at the LAST `)`; ppid is the second field
        // after it, starttime index 19, both from this one read.
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let comm = stat.get(open + 1..close).unwrap_or_default().to_owned();
        let post: Vec<&str> = stat.get(close + 2..).unwrap_or_default().split_whitespace().collect();
        let Some(ppid) = post.get(1).and_then(|p| p.parse().ok()) else {
            continue;
        };
        let starttime = post.get(19).map(|s| (*s).to_owned()).unwrap_or_default();
        let token = std::fs::read(format!("/proc/{pid}/environ"))
            .ok()
            .and_then(|env| marker_in_environ(&env));
        table.insert(pid, ProcRow { ppid, comm, starttime, token });
    }
    table
}

/// Whether `ancestor` appears on `pid`'s parent chain (not counting `pid`).
fn has_ancestor(table: &HashMap<u32, ProcRow>, pid: u32, ancestor: u32) -> bool {
    let mut cursor = pid;
    // Bounded: a consistent snapshot has no parent cycle, but a pid reused
    // mid-scan could fake one.
    for _ in 0..256 {
        let Some(row) = table.get(&cursor) else { return false };
        if row.ppid == ancestor {
            return true;
        }
        if row.ppid == 0 || row.ppid == cursor {
            return false;
        }
        cursor = row.ppid;
    }
    false
}

/// Every process carrying a dead token, excluding `me`, its ancestors and its
/// descendants, newest first. Pure over the table so the selection is
/// unit-testable.
fn select(table: &HashMap<u32, ProcRow>, dead: &HashSet<String>, me: u32) -> Vec<Orphan> {
    let mut out: Vec<Orphan> = table
        .iter()
        .filter_map(|(&pid, row)| {
            let token = row.token.as_ref().filter(|t| dead.contains(*t))?;
            if pid == me || has_ancestor(table, pid, me) || has_ancestor(table, me, pid) {
                return None;
            }
            Some(Orphan {
                pid,
                comm: row.comm.clone(),
                starttime: row.starttime.clone(),
                token: token.clone(),
            })
        })
        .collect();
    // A child starts after its parent, so newest first is leaves first: a
    // test binary dies before a surviving parent could notice and react.
    out.sort_by(|a, b| {
        let a_start = a.starttime.parse::<u64>().unwrap_or(0);
        let b_start = b.starttime.parse::<u64>().unwrap_or(0);
        b_start.cmp(&a_start).then(a.pid.cmp(&b.pid))
    });
    out
}

/// SIGKILL `pid` if it still has the starttime it was selected with.
fn sigkill_verified(pid: u32, starttime: &str) -> bool {
    if starttime.is_empty()
        || crate::lockfile::proc_starttime(pid).as_deref() != Some(starttime)
    {
        return false;
    }
    // SAFETY: identity re-verified immediately above; ESRCH is benign, and the
    // residual window is the one every pid-addressed signal has.
    unsafe { libc::kill(pid.cast_signed(), libc::SIGKILL) == 0 }
}

/// What one reap did.
#[derive(Debug, Default)]
struct ReapReport {
    /// The comm of every process signalled.
    killed: Vec<String>,
    /// Carriers still visible after the last pass; their files stay.
    survivors: usize,
}

/// Whether `pid` is still running: present in `/proc` and neither a zombie nor
/// dead. A zombie's environment reads empty, so it no longer counts as a
/// carrier anyway; this only decides how long [`settle`] waits.
fn is_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat
        .rfind(')')
        .and_then(|close| stat.get(close + 2..))
        .and_then(|post| post.split_whitespace().next());
    !matches!(state, None | Some("Z" | "X"))
}

/// Wait, briefly, for signalled processes to finish dying, so the rescan that
/// decides whether a file may be removed does not see a process mid-exit.
fn settle(pids: &[u32]) {
    let deadline = std::time::Instant::now() + SETTLE_BUDGET;
    while pids.iter().any(|&pid| is_running(pid)) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Claim the dead registrations under `dir`, kill their carriers, and remove
/// each file whose token no longer has a carrier. `me` is the process whose
/// ancestors and descendants are exempt (see the module header); `u32::MAX`
/// exempts nothing.
fn reap_in(dir: &Path, me: u32) -> ReapReport {
    let mut report = ReapReport::default();
    let dead = claim_dead(dir);
    if dead.is_empty() {
        return report;
    }
    let tokens: HashSet<String> = dead.iter().map(|d| d.token.clone()).collect();

    let mut signalled: HashSet<u32> = HashSet::new();
    let mut remaining = select(&read_table(), &tokens, me);
    for _ in 0..REAP_PASSES {
        if remaining.is_empty() {
            break;
        }
        let mut this_pass = Vec::new();
        for orphan in &remaining {
            if sigkill_verified(orphan.pid, &orphan.starttime) {
                this_pass.push(orphan.pid);
                if signalled.insert(orphan.pid) {
                    report.killed.push(orphan.comm.clone());
                }
            }
        }
        settle(&this_pass);
        remaining = select(&read_table(), &tokens, me);
    }
    // Whatever is still visible (a process stuck in uninterruptible sleep, a
    // child forked after the last scan) keeps its token's file, and the next
    // reap tries again.
    report.survivors = remaining.len();
    let alive: HashSet<&str> = remaining.iter().map(|o| o.token.as_str()).collect();
    for run in &dead {
        if !alive.contains(run.token.as_str()) {
            drop(std::fs::remove_file(&run.path));
        }
    }
    report
}

/// The reap's one-line report: comm counts, no pids (everything named is dead).
fn reap_line(report: &ReapReport) -> String {
    let mut comms: Vec<(&str, usize)> = Vec::new();
    for c in &report.killed {
        match comms.iter_mut().find(|(name, _)| name == c) {
            Some((_, n)) => *n += 1,
            None => comms.push((c, 1)),
        }
    }
    let comms: Vec<String> = comms
        .iter()
        .map(|(c, n)| if *n > 1 { format!("{c} x{n}") } else { (*c).to_owned() })
        .collect();
    let mut line = format!(
        "SIGKILL sent to {} ({}) that outlived the brokkr run that started them",
        output::count(report.killed.len(), "orphaned test process"),
        comms.join(", "),
    );
    if report.survivors > 0 {
        line.push_str(&format!(
            "; {} still visible, retried at the next hold",
            output::count(report.survivors, "process")
        ));
    }
    line.push_str(" (`brokkr man check strays`)");
    line
}

/// The orphan reap every fresh hold runs (`lockfile::acquire`'s fresh-hold
/// path, beside the stray reap). Nothing found prints nothing; like the stray
/// reap it is hygiene on the way to the real work, never a gate on it.
///
/// Inert in unit tests, like [`marker`]: a test must never signal a host
/// process or delete a file under the real `~/.brokkr`. The reap itself is
/// tested against a scratch directory through [`reap_in`].
pub fn reap_after_lock() {
    if cfg!(test) {
        return;
    }
    let Some(dir) = runs_dir() else { return };
    let report = reap_in(&dir, std::process::id());
    if report.killed.is_empty() && report.survivors == 0 {
        return;
    }
    output::lock_msg(&reap_line(&report));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn row(ppid: u32, comm: &str, starttime: u64, token: Option<&str>) -> ProcRow {
        ProcRow {
            ppid,
            comm: comm.to_owned(),
            starttime: starttime.to_string(),
            token: token.map(str::to_owned),
        }
    }

    const DEAD: &str = "0123456789abcdef0123456789abcdef";
    const LIVE: &str = "fedcba9876543210fedcba9876543210";

    fn dead_set() -> HashSet<String> {
        HashSet::from([DEAD.to_owned()])
    }

    #[test]
    fn the_marker_is_read_from_a_raw_environ_block() {
        let env = b"PATH=/bin\0BROKKR_TEST_RUN=abc\0BROKKR_TEST_RUN=later\0HOME=/h\0";
        assert_eq!(marker_in_environ(env).as_deref(), Some("abc"));
        assert_eq!(marker_in_environ(b"PATH=/bin\0NOT_BROKKR_TEST_RUN=x\0"), None);
        // A prefix of the name is not the name.
        assert_eq!(marker_in_environ(b"BROKKR_TEST_RUNNER=x\0"), None);
        assert_eq!(marker_in_environ(b""), None);
    }

    #[test]
    fn only_minted_shapes_are_tokens() {
        assert!(is_token(DEAD));
        assert!(!is_token("0123456789ABCDEF0123456789ABCDEF"));
        assert!(!is_token("0123"));
        assert!(!is_token("../../../../etc/passwd0123456789a"));
    }

    /// The shape an OOM-killed brokkr leaves behind: brokkr and cargo are gone,
    /// the test binary and whatever it spawned were reparented to init. Both carry the
    /// dead token and die, newest first; a live run's tree is untouched.
    #[test]
    fn carriers_of_a_dead_token_are_selected_leaves_first() {
        let table = HashMap::from([
            (1, row(0, "systemd", 1, None)),
            (50, row(1, "my_crate-3f2a", 500, Some(DEAD))),
            (51, row(50, "sh", 510, Some(DEAD))),
            (60, row(1, "brokkr", 600, None)),
            (61, row(60, "cargo", 610, Some(LIVE))),
            (62, row(61, "other_test-9a", 620, Some(LIVE))),
        ]);
        let picked: Vec<u32> = select(&table, &dead_set(), 999_999).iter().map(|o| o.pid).collect();
        assert_eq!(picked, vec![51, 50]);
    }

    /// The mark is inherited, so a brokkr started from inside a test carries
    /// the outer run's token, and so do its own non-test children. When the
    /// outer run is the dead one, the reaping brokkr must spare itself, the
    /// orphaned test binary it runs under, and its own cargo.
    #[test]
    fn the_reaper_spares_its_ancestors_and_descendants() {
        let me = 70;
        let table = HashMap::from([
            (1, row(0, "systemd", 1, None)),
            (50, row(1, "my_crate-3f2a", 500, Some(DEAD))), // our orphaned parent
            (me, row(50, "brokkr", 700, Some(DEAD))),       // us
            (71, row(me, "cargo", 710, Some(DEAD))),         // our own build
            (80, row(1, "sibling_test", 800, Some(DEAD))),   // a real orphan
        ]);
        let picked: Vec<u32> = select(&table, &dead_set(), me).iter().map(|o| o.pid).collect();
        assert_eq!(picked, vec![80]);
    }

    #[test]
    fn a_live_registration_is_not_claimed_and_a_dead_one_is() {
        let dir = crate::test_scratch::scratch("test_orphans", "claim");
        let live = register_in(&dir).unwrap();
        let gone = register_in(&dir).unwrap();
        let gone_token = gone.token.clone();
        drop(gone);

        // Retried for the fork window described in the end-to-end test below.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let dead = loop {
            let dead = claim_dead(&dir);
            if !dead.is_empty() || std::time::Instant::now() >= deadline {
                break dead;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let tokens: Vec<&str> = dead.iter().map(|d| d.token.as_str()).collect();
        assert_eq!(tokens, vec![gone_token.as_str()], "only the dropped registration is dead");
        assert!(dir.join(format!("{}.{LIVE_EXT}", live.token)).exists());
        assert!(
            !dir.join(format!("{}.{PENDING_EXT}", live.token)).exists(),
            "a completed registration leaves no pending file"
        );
    }

    #[test]
    fn a_fresh_pending_file_survives_and_foreign_names_are_ignored() {
        let dir = crate::test_scratch::scratch("test_orphans", "pending");
        let pending = dir.join(format!("{DEAD}.{PENDING_EXT}"));
        std::fs::write(&pending, "").unwrap();
        let foreign = dir.join("notes.lock");
        std::fs::write(&foreign, "").unwrap();
        assert!(claim_dead(&dir).is_empty());
        assert!(pending.exists(), "a pending file inside its grace period is a registration in flight");
        assert!(foreign.exists());
    }

    /// End to end on a process this test owns: a registration dies, a process
    /// carrying its token is SIGKILLed by the reap, and the file is removed
    /// once nothing carries the token. The token is random, so nothing else on
    /// the host can match it.
    #[test]
    fn a_carrier_of_a_dead_registration_is_killed_and_the_file_removed() {
        use std::process::{Command, Stdio};

        let dir = crate::test_scratch::scratch("test_orphans", "reap");
        let reg = register_in(&dir).unwrap();
        let token = reg.token.clone();
        let mut child = Command::new("sleep")
            .arg("30")
            .env(MARKER_ENV, &token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        // Between fork and exec the child still shows the parent's
        // environment; wait until the marked one is visible.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::fs::read(format!("/proc/{pid}/environ"))
            .ok()
            .and_then(|e| marker_in_environ(&e))
            .as_deref()
            != Some(token.as_str())
        {
            assert!(std::time::Instant::now() < deadline, "child never exec'd");
            std::thread::sleep(Duration::from_millis(5));
        }

        // The sleep is this test process's child, which the production reap
        // exempts; `u32::MAX` exempts nothing, standing in for a reaper that
        // is not the dead run's relative.
        //
        // Alive: nothing is claimed and the child is untouched.
        assert!(reap_in(&dir, u32::MAX).killed.is_empty());
        assert!(child.try_wait().unwrap().is_none());

        drop(reg);
        // A sibling test forking at the moment of the drop briefly holds a
        // copy of the registration fd (until its exec closes it), and with it
        // the flock - the safe direction, a dead run read as live. Retry past
        // that window rather than flake on it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let report = loop {
            let report = reap_in(&dir, u32::MAX);
            if !report.killed.is_empty() || std::time::Instant::now() >= deadline {
                break report;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let status = child.wait().unwrap();
        assert_eq!(report.killed, vec!["sleep".to_owned()], "signalled once, counted once");
        assert!(!status.success(), "the carrier must have been killed");
        // `child` is a zombie until `wait` above, and a zombie's environ reads
        // empty, so the reap saw no survivor and removed the file.
        assert_eq!(report.survivors, 0);
        assert!(!dir.join(format!("{token}.{LIVE_EXT}")).exists());
    }

    #[test]
    fn the_reap_line_counts_comms_and_names_survivors() {
        let report = ReapReport {
            killed: vec!["my_crate-3f2a".into(), "sh".into(), "sh".into()],
            survivors: 1,
        };
        assert_eq!(
            reap_line(&report),
            "SIGKILL sent to 3 orphaned test processes (my_crate-3f2a, sh x2) that outlived the \
             brokkr run that started them; 1 process still visible, retried at the next hold \
             (`brokkr man check strays`)"
        );
    }
}
