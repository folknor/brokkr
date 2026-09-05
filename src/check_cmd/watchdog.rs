// The time ceilings on `brokkr check`: one on the whole run, one per phase.
//
// Every child `check` spawns already has a per-invocation deadline (the
// 20s hung-test watchdog, the captured runner's deadline), but nothing
// bounded a phase or the run as a whole: a clippy invocation that never
// returns (observed: 1h15m on `clippy threaded/default`), a linker that
// hangs, or a test binary that keeps forking could hold the global lock for
// hours. These are the outer bounds - clocks armed after the lock is taken,
// and a forceful `brokkr kill --hard` equivalent when one fires.
//
// Forceful, not cooperative: the cooperative path (`SigtermGuard` +
// `Interrupted`) depends on whichever runner is currently polling the
// flag, and a run that has already overrun by this much is exactly the
// one whose runner may not be polling. So the watchdog SIGKILLs every
// descendant of brokkr found in `/proc` (children of `check` are spawned
// into their own process groups, so a group signal at brokkr's PG would
// miss them, and not all of them are published to the lockfile) and then
// exits the process. The flock is released by the kernel on exit; lockfile
// readers verify the holder's identity token and fail closed on the stale
// metadata, as they do after `brokkr kill --hard`.

/// How long a `brokkr check` run may hold the lock before the watchdog
/// kills it. Measured from lock acquisition, so a wait behind another brokkr
/// command does not count.
pub(crate) const CHECK_CEILING: std::time::Duration = std::time::Duration::from_secs(25 * 60);

/// Exit status of a run the watchdog killed. `timeout(1)`'s convention.
pub(crate) const WATCHDOG_EXIT_CODE: i32 = 124;

/// The per-phase ceilings, by `PHASE_NAMES` identifier. The source-reading
/// phases finish in seconds, so two minutes is already generous; the build
/// phases get what a cold store plausibly needs and no more. Clippy's five
/// minutes is set against the observed hang, not against its normal cost
/// (about 20s warm on the largest consuming workspace).
fn phase_ceiling(phase: &str) -> std::time::Duration {
    let minutes = match phase {
        "clippy" => 5,
        "test" => 15,
        "coverage" | "install_feature" | "script_check" => 5,
        _ => 2,
    };
    std::time::Duration::from_secs(minutes * 60)
}

/// The phase in flight and when it started. `None` outside a phase (before
/// the first, or after the watchdog is disarmed).
static PHASE_CLOCK: std::sync::Mutex<Option<(&'static str, std::time::Instant)>> =
    std::sync::Mutex::new(None);

/// Mark `phase` as the phase in flight: points `failing_phase` at it (the
/// summary's `failed_phase` on an error) and restarts the phase clock. Every
/// phase entry in `run_convention_phases` / `run_build_phases` goes through
/// here, so the two bookkeepings cannot drift.
pub(crate) fn begin_phase(failing_phase: &mut Option<&'static str>, phase: &'static str) {
    *failing_phase = Some(phase);
    if let Ok(mut clock) = PHASE_CLOCK.lock() {
        *clock = Some((phase, std::time::Instant::now()));
    }
}

/// Arms the ceilings; dropping it disarms. Hold it for the whole of `cmd_check`.
pub(crate) struct CheckWatchdog {
    disarm: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl CheckWatchdog {
    pub(crate) fn arm(limit: std::time::Duration) -> Self {
        let disarm = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&disarm);
        let started = std::time::Instant::now();
        std::thread::Builder::new()
            .name("check-watchdog".into())
            .spawn(move || {
                // Poll rather than one long sleep so a disarm releases the
                // thread promptly instead of leaving it parked until exit.
                loop {
                    if flag.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    if started.elapsed() >= limit {
                        fire(&format!(
                            "check exceeded its {} whole-run ceiling",
                            crate::lockfile::format_duration(limit.as_secs()),
                        ));
                    }
                    let overrun = PHASE_CLOCK.lock().ok().and_then(|clock| {
                        clock.and_then(|(phase, since)| {
                            let ceiling = phase_ceiling(phase);
                            (since.elapsed() >= ceiling).then_some((phase, ceiling))
                        })
                    });
                    if let Some((phase, ceiling)) = overrun {
                        fire(&format!(
                            "check's {phase} phase exceeded its {} ceiling",
                            crate::lockfile::format_duration(ceiling.as_secs()),
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            })
            .ok();
        Self { disarm }
    }
}

impl Drop for CheckWatchdog {
    fn drop(&mut self) {
        self.disarm.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut clock) = PHASE_CLOCK.lock() {
            *clock = None;
        }
    }
}

/// A ceiling was hit: report, SIGKILL every descendant, exit.
fn fire(why: &str) -> ! {
    output::error(&format!(
        "{why} - killing the run (as `brokkr kill --hard` would)"
    ));
    // SAFETY: getpid takes no arguments and cannot fail.
    let me = unsafe { libc::getpid() }.cast_unsigned();
    // Two sweeps: a descendant killed mid-fork can leave a child that the
    // first walk did not see. Anything spawned between sweeps is a
    // grandchild of a dead parent and reparents away from our subtree, which
    // is the residual `brokkr kill --hard` accepts too.
    let mut killed = 0usize;
    for _ in 0..2 {
        for pid in descendants(me) {
            // SAFETY: SIGKILL to a PID we just read as our descendant. ESRCH
            // (already gone) is benign; the recycling window between the
            // read and the signal is the same one `kill --hard` accepts for
            // PIDs it cannot pidfd-pin.
            if unsafe { libc::kill(pid.cast_signed(), libc::SIGKILL) } == 0 {
                killed += 1;
            }
        }
    }
    output::error(&format!(
        "SIGKILL sent to {killed} descendant process(es); brokkr exiting {WATCHDOG_EXIT_CODE}",
    ));
    std::process::exit(WATCHDOG_EXIT_CODE)
}

/// Every live process whose parent chain reaches `root`, deepest first, from
/// one read of `/proc`. Deepest first so a leaf dies before its parent can
/// notice and respawn it.
fn descendants(root: u32) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if let Some(ppid) = proc_ppid(pid) {
            children.entry(ppid).or_default().push(pid);
        }
    }
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if pid != root {
            out.push(pid);
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }
    out.reverse();
    out
}

/// The parent PID from `/proc/<pid>/stat`: the first field after the
/// parenthesised comm (which may itself contain spaces and parentheses, hence
/// the `rfind`) and the one-char state.
fn proc_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let comm_end = stat.rfind(')')?;
    let mut fields = stat[comm_end + 2..].split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

#[cfg(test)]
mod watchdog_tests {
    use super::*;

    #[test]
    fn own_parent_is_a_descendant_of_grandparent() {
        // SAFETY: plain getter.
        let me = unsafe { libc::getpid() }.cast_unsigned();
        let parent = proc_ppid(me).expect("own ppid");
        let grandparent = proc_ppid(parent).expect("parent's ppid");
        if grandparent == 0 {
            return; // init as parent: no grandparent to walk from
        }
        assert!(descendants(grandparent).contains(&me));
    }

    #[test]
    fn descendants_exclude_the_root() {
        // SAFETY: plain getter.
        let me = unsafe { libc::getpid() }.cast_unsigned();
        assert!(!descendants(me).contains(&me));
    }

    #[test]
    fn clippy_ceiling_is_five_minutes() {
        assert_eq!(phase_ceiling("clippy"), std::time::Duration::from_secs(300));
        assert!(phase_ceiling("test") > phase_ceiling("clippy"));
        assert!(CHECK_CEILING > phase_ceiling("test"));
    }

    #[test]
    fn begin_phase_sets_both_bookkeepings() {
        let mut failing = None;
        begin_phase(&mut failing, "gremlins");
        assert_eq!(failing, Some("gremlins"));
        let clock = PHASE_CLOCK.lock().expect("clock");
        assert!(matches!(*clock, Some(("gremlins", _))));
    }
}
