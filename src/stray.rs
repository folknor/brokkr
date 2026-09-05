//! Stray cargo processes: cargo, rustc, clippy, rustdoc and build scripts
//! running with no brokkr ancestor.
//!
//! brokkr's premise is that every cargo invocation on a development host goes
//! through it - that is what the global lock serializes and what the
//! measurement stores assume. A cargo that runs outside brokkr breaks that in
//! two ways: it competes for CPU with whatever brokkr is measuring, and it
//! takes cargo's own build-directory lock, on which brokkr's cargo then
//! blocks with nothing to time out. Both happened: a rust-analyzer `cargo
//! check` (nightly regression, 2026-09) sat on the target lock for over an
//! hour, and the next day four of its build scripts spun at 100% CPU under
//! it, parking every brokkr command behind them.
//!
//! So a locked brokkr command reaps strays right after it takes the lock
//! ([`reap_after_lock`]), and `brokkr strays` lists or kills them by hand.
//! Reaping is SIGKILL: a build script or rustc has no cleanup worth waiting
//! for, and a cargo killed under rust-analyzer is simply re-run by it later.
//! Attribution is by ancestry read from `/proc` - the nearest ancestor that
//! is not itself part of the cargo family (`rust-analyzer`, a shell, an
//! editor), so the report says who started it.

use std::collections::HashMap;

use crate::error::DevError;
use crate::output;

/// One cargo-family process with no brokkr ancestor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stray {
    pub pid: u32,
    /// `/proc/<pid>/comm`, kernel-truncated to 15 bytes.
    pub comm: String,
    /// Depth below the process tree's root, used to kill leaves first.
    pub depth: usize,
    /// The nearest ancestor outside the cargo family: `<comm> (pid N)`, or
    /// `unknown` when the chain ends before one is found.
    pub started_by: String,
    /// That ancestor's pid and comm, when found - the starter.
    pub starter: Option<(u32, String)>,
}

/// The starters that die with their strays: rust-analyzer, and only it. A
/// killed cargo under rust-analyzer is re-run by it within seconds, so the
/// reap would be a loop; the editor restarts rust-analyzer on demand, so the
/// cost is a re-index. A shell or editor as starter (a hand-typed `cargo`)
/// is never signalled - killing the user's terminal to stop a build is not a
/// trade anyone asked for. Deduplicated by pid.
pub fn starters_to_kill(strays: &[Stray]) -> Vec<(u32, String)> {
    let mut out: Vec<(u32, String)> = Vec::new();
    for s in strays {
        if let Some((pid, comm)) = &s.starter
            && comm.starts_with("rust-analyzer")
            && !out.iter().any(|(p, _)| p == pid)
        {
            out.push((*pid, comm.clone()));
        }
    }
    out
}

/// Whether a `comm` belongs to the cargo family: the processes only brokkr
/// should be running. Comm is truncated to 15 bytes by the kernel, so build
/// scripts read `build_script_bu` / `build-script-bu`; prefix-matched.
pub fn is_cargo_family(comm: &str) -> bool {
    comm == "cargo"
        || comm.starts_with("cargo-")
        || comm.starts_with("rustc")
        || comm.starts_with("rustdoc")
        || comm.starts_with("clippy-driver")
        || comm.starts_with("build_script")
        || comm.starts_with("build-script")
}

struct ProcEntry {
    ppid: u32,
    comm: String,
}

fn read_proc() -> HashMap<u32, ProcEntry> {
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
        // `comm` sits in parentheses and may contain spaces or parentheses
        // itself, so split at the LAST `)`; ppid is the second field after it.
        let Some(open) = stat.find('(') else { continue };
        let Some(close) = stat.rfind(')') else { continue };
        let comm = stat[open + 1..close].to_owned();
        let mut fields = stat[close + 2..].split_whitespace();
        let _state = fields.next();
        let Some(ppid) = fields.next().and_then(|p| p.parse().ok()) else {
            continue;
        };
        table.insert(pid, ProcEntry { ppid, comm });
    }
    table
}

/// Every cargo-family process with no brokkr ancestor, leaves first. Pure
/// over the given table so the classification is unit-testable.
fn classify(table: &HashMap<u32, ProcEntry>) -> Vec<Stray> {
    let mut strays = Vec::new();
    for (&pid, entry) in table {
        if !is_cargo_family(&entry.comm) {
            continue;
        }
        let mut under_brokkr = false;
        let mut started_by = None;
        let mut starter = None;
        let mut depth = 0usize;
        let mut cursor = entry.ppid;
        // Bounded walk: a `/proc` snapshot can contain a ppid cycle only if
        // the table is inconsistent, but a bound costs nothing.
        for _ in 0..256 {
            let Some(parent) = table.get(&cursor) else { break };
            depth += 1;
            if parent.comm == "brokkr" {
                under_brokkr = true;
                break;
            }
            if started_by.is_none() && !is_cargo_family(&parent.comm) {
                started_by = Some(format!("{} (pid {cursor})", parent.comm));
                starter = Some((cursor, parent.comm.clone()));
            }
            if cursor == 0 || cursor == parent.ppid {
                break;
            }
            cursor = parent.ppid;
        }
        if under_brokkr {
            continue;
        }
        strays.push(Stray {
            pid,
            comm: entry.comm.clone(),
            depth,
            started_by: started_by.unwrap_or_else(|| "unknown".to_owned()),
            starter,
        });
    }
    // Deepest first, so a build script dies before the cargo that would
    // otherwise notice and respawn it; pid as the tiebreak for stable output.
    strays.sort_by(|a, b| b.depth.cmp(&a.depth).then(a.pid.cmp(&b.pid)));
    strays
}

/// The strays on this host right now, leaves first.
pub fn find() -> Vec<Stray> {
    classify(&read_proc())
}

/// SIGKILL each stray, then each rust-analyzer starter. Returns `(strays,
/// starters)` signalled; a process that exited between the scan and the
/// signal is not counted and not an error.
pub fn kill(strays: &[Stray]) -> (usize, usize) {
    let sigkill = |pid: u32| {
        // SAFETY: SIGKILL to a PID read from `/proc` moments ago; the
        // recycling window is the one every PID-addressed signal has, and
        // ESRCH is benign.
        unsafe { libc::kill(pid.cast_signed(), libc::SIGKILL) == 0 }
    };
    let killed = strays.iter().filter(|s| sigkill(s.pid)).count();
    let starters = starters_to_kill(strays)
        .into_iter()
        .filter(|(pid, _)| sigkill(*pid))
        .count();
    (killed, starters)
}

/// The `SIGKILL sent to …` line.
fn killed_line(killed: usize, starters: usize) -> String {
    let mut line = format!("SIGKILL sent to {}", output::count(killed, "stray cargo process"));
    if starters > 0 {
        line.push_str(&format!(" and {} (it would only re-run the cargo)", output::count(starters, "rust-analyzer")));
    }
    line
}

/// One report line per stray.
pub fn describe(s: &Stray) -> String {
    format!("{} (pid {}) started by {}", s.comm, s.pid, s.started_by)
}

/// The reap every locked command runs once it holds the lock: find, report,
/// kill. Nothing found prints nothing. Failure to read `/proc` reads as
/// nothing found - the reap is a convenience on the way to the real work,
/// never a gate on it.
pub fn reap_after_lock() {
    let strays = find();
    if strays.is_empty() {
        return;
    }
    for s in &strays {
        output::lock_msg(&format!("stray cargo process: {}", describe(s)));
    }
    let (killed, starters) = kill(&strays);
    output::lock_msg(&format!(
        "{} - no cargo runs outside brokkr on a brokkr host (`brokkr man check strays`)",
        killed_line(killed, starters),
    ));
}

/// `brokkr strays [--kill]`: bare lists, `--kill` lists then kills.
pub fn cmd_strays(kill_them: bool) -> Result<(), DevError> {
    let strays = find();
    if strays.is_empty() {
        output::lock_msg("no stray cargo processes");
        return Ok(());
    }
    for s in &strays {
        output::lock_msg(&describe(s));
    }
    if kill_them {
        let (killed, starters) = kill(&strays);
        output::lock_msg(&killed_line(killed, starters));
    } else {
        output::lock_msg("`brokkr strays --kill` sends SIGKILL; every locked brokkr command does so on its own once it holds the lock");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(rows: &[(u32, u32, &str)]) -> HashMap<u32, ProcEntry> {
        rows.iter()
            .map(|&(pid, ppid, comm)| (pid, ProcEntry { ppid, comm: comm.to_owned() }))
            .collect()
    }

    #[test]
    fn cargo_under_brokkr_is_not_a_stray() {
        let t = table(&[(1, 0, "systemd"), (10, 1, "zsh"), (20, 10, "brokkr"), (30, 20, "cargo"), (40, 30, "rustc")]);
        assert!(classify(&t).is_empty());
    }

    #[test]
    fn rust_analyzer_cargo_and_its_build_scripts_are_strays_leaves_first() {
        let t = table(&[
            (1, 0, "systemd"),
            (10, 1, "rust-analyzer"),
            (20, 10, "cargo"),
            (30, 20, "build_script_bu"),
            (31, 20, "build_script_bu"),
        ]);
        let strays = classify(&t);
        let pids: Vec<u32> = strays.iter().map(|s| s.pid).collect();
        assert_eq!(pids, vec![30, 31, 20]);
        assert!(strays.iter().all(|s| s.started_by == "rust-analyzer (pid 10)"), "{strays:?}");
    }

    #[test]
    fn comm_family_matches_truncated_build_script_names() {
        assert!(is_cargo_family("build_script_bu"));
        assert!(is_cargo_family("build-script-bu"));
        assert!(is_cargo_family("clippy-driver"));
        assert!(is_cargo_family("cargo-clippy"));
        assert!(!is_cargo_family("rust-analyzer"));
        assert!(!is_cargo_family("brokkr"));
    }

    #[test]
    fn rust_analyzer_starter_is_killed_with_its_cargo_a_shell_is_not() {
        let t = table(&[
            (1, 0, "systemd"),
            (10, 1, "rust-analyzer"),
            (20, 10, "cargo"),
            (11, 1, "zsh"),
            (21, 11, "cargo"),
        ]);
        let strays = classify(&t);
        let starters: Vec<u32> = starters_to_kill(&strays).into_iter().map(|(pid, _)| pid).collect();
        assert_eq!(starters, vec![10]);
    }

    #[test]
    fn own_process_tree_reads_without_panicking() {
        // Whatever is running on the test host, the scan must be total.
        let _ = find();
    }
}
