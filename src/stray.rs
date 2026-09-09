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
//!
//! Ownership - the question of which cargo is brokkr's own and therefore not a
//! stray - is decided by the *verified lock holder's pid*, not by an ancestor
//! being named `brokkr`; see [`Ownership`] for why the name is not enough.

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
        // The rustc wrapper. `brokkr-rustc-guard` truncates to
        // `brokkr-rustc-gu`, which matched nothing here, so the guard was
        // never a reap candidate - and killing its parent cargo does not kill
        // it, because the reap signals the PIDs it selected rather than their
        // descendant trees. A guard that outlived the reap went on to exec
        // rustc, which is the one thing the reap exists to prevent. Matched on
        // the `brokkr-rustc` prefix, which cannot collide with `brokkr`
        // itself.
        || comm.starts_with("brokkr-rustc")
}

/// How a reap decides that a cargo-family process is brokkr's own work rather
/// than a stray.
///
/// Ancestry by `comm` was the original rule and it is spoofable in the way that
/// matters: any executable *named* `brokkr` exempts every cargo and rustc
/// beneath it, with no reference to whether it holds anything. A shell copied
/// to that name is enough, which is not a hypothetical - it is how the guard's
/// own fence was first probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    /// The verified current lock holder's PID. An ancestor matching it is
    /// brokkr's own work; a process merely *named* `brokkr` is not. This is
    /// the rule whenever a hold is active, which is exactly when a stray can
    /// do damage.
    Holder(u32),
    /// No hold is active, or the holder's identity could not be verified
    /// (a PID namespace, a stale record). Fall back to the ancestor's `comm`.
    /// Spoofable, and chosen anyway: the alternative - exempting nothing -
    /// SIGKILLs brokkr's own build the moment verification is unavailable,
    /// and this reaper runs unattended inside every locked command.
    UnverifiedComm,
}

/// The ownership rule in force right now, read from the lock file.
fn current_ownership() -> Ownership {
    match crate::lockfile::status() {
        Ok(Some(info))
            if crate::lockfile::verify_identity(info.pid, &info.starttime, &info.boot_id) =>
        {
            Ownership::Holder(info.pid)
        }
        _ => Ownership::UnverifiedComm,
    }
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

/// Every cargo-family process not owned by brokkr, leaves first. Pure over the
/// given table and ownership rule so the classification is unit-testable.
fn classify(table: &HashMap<u32, ProcEntry>, ownership: Ownership) -> Vec<Stray> {
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
            let owns = match ownership {
                Ownership::Holder(holder) => cursor == holder,
                Ownership::UnverifiedComm => parent.comm == "brokkr",
            };
            if owns {
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

/// Every descendant of `pid` in the table, paired with its depth below it.
///
/// Bounded by the table size: a `/proc` snapshot can only contain a parent cycle
/// if it is internally inconsistent, and `seen` makes that terminate anyway.
fn collect_descendants(
    table: &HashMap<u32, ProcEntry>,
    pid: u32,
    depth: usize,
    out: &mut Vec<(usize, u32)>,
) {
    if depth > 64 {
        return;
    }
    for (&child, entry) in table {
        if entry.ppid == pid && child != pid {
            out.push((depth + 1, child));
            collect_descendants(table, child, depth + 1, out);
        }
    }
}

/// The strays on this host right now, leaves first.
pub fn find() -> Vec<Stray> {
    classify(&read_proc(), current_ownership())
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
    // Descendants of each stray, deepest first, before the strays themselves.
    //
    // Signalling only the selected PIDs left a hole: killing a cargo does not
    // kill the rustc-wrapper beneath it, so a wrapper that had already been
    // admitted survived the reap and went on to exec a compiler - the single
    // thing the reap exists to prevent. The wrapper is in the cargo family now,
    // so it is usually selected on its own, but "usually" is not a property: a
    // stray can have children of any name, and a build script's children are
    // arbitrary programs.
    //
    // Re-read `/proc` rather than reusing the classification snapshot, so a
    // child created since the scan is still found. This is not atomic
    // containment - a process can fork again between this read and the signal -
    // so it narrows the hole rather than closing it. Closing it needs a cgroup,
    // which is a bigger change than this reaper is.
    let table = read_proc();
    let mut descendants: Vec<(usize, u32)> = Vec::new();
    for s in strays {
        collect_descendants(&table, s.pid, 0, &mut descendants);
    }
    // Deepest first, and never a PID that is itself a listed stray - those are
    // signalled below, in the caller's leaves-first order.
    descendants.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, pid) in &descendants {
        if !strays.iter().any(|s| s.pid == *pid) {
            sigkill(*pid);
        }
    }
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

/// The reap's whole report, one line however many strays there were - the
/// reap runs at the top of every locked command, so its output is overhead
/// paid on every invocation. Comms are counted (`build_script_bu x4`),
/// starters deduplicated. No pids: everything named is dead by the time the
/// line prints, so a pid identifies nothing. The live, addressable detail
/// stays with `brokkr strays`.
fn reap_line(strays: &[Stray], killed: usize, starters: usize) -> String {
    let mut comms: Vec<(&str, usize)> = Vec::new();
    for s in strays {
        match comms.iter_mut().find(|(c, _)| *c == s.comm) {
            Some((_, n)) => *n += 1,
            None => comms.push((&s.comm, 1)),
        }
    }
    let comms: Vec<String> = comms
        .iter()
        .map(|(c, n)| if *n > 1 { format!("{c} x{n}") } else { (*c).to_owned() })
        .collect();
    let mut by: Vec<&str> = Vec::new();
    for s in strays {
        let name = s.starter.as_ref().map_or("unknown", |(_, comm)| comm);
        if !by.contains(&name) {
            by.push(name);
        }
    }
    let mut line = format!(
        "SIGKILL sent to {} ({}) started by {}",
        output::count(killed, "stray cargo process"),
        comms.join(", "),
        by.join(", "),
    );
    if starters > 0 {
        line.push_str(&format!(" and to {}", output::count(starters, "rust-analyzer")));
    }
    line.push_str(" (`brokkr man check strays`)");
    line
}

/// One report line per stray.
pub fn describe(s: &Stray) -> String {
    format!("{} (pid {}) started by {}", s.comm, s.pid, s.started_by)
}

/// The reap every locked command runs once it holds the lock: find, kill,
/// report on one line. Nothing found prints nothing. Failure to read `/proc`
/// reads as nothing found - the reap is a convenience on the way to the real
/// work, never a gate on it.
pub fn reap_after_lock() {
    let strays = find();
    if strays.is_empty() {
        return;
    }
    let (killed, starters) = kill(&strays);
    output::lock_msg(&reap_line(&strays, killed, starters));
}

/// The reap run from inside lock acquisition, when compilation leases have not
/// drained on their own.
///
/// Separate from [`reap_after_lock`] because it runs at a different moment and
/// carries a different meaning. `reap_after_lock` happens once the hold is
/// usable and is pure hygiene. This one runs *during* acquisition, while
/// `brokkr.lock` is held but the compile lease has not been obtained, and its
/// purpose is to unblock a drain that a foreign compiler is sitting on.
///
/// It is a recovery attempt, never a guarantee: a lease can be held by a
/// detached process with a name no cargo-family scan matches, and killing every
/// process this finds does not prove the lease was released. The drain's only
/// evidence remains the exclusive flock itself.
pub fn reap_for_drain() {
    let strays = find();
    if strays.is_empty() {
        output::lock_msg("no strays found - the lease is held by something this scan cannot see");
        return;
    }
    let (killed, starters) = kill(&strays);
    output::lock_msg(&reap_line(&strays, killed, starters));
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
        assert!(classify(&t, Ownership::Holder(20)).is_empty());
        // And under the fallback rule, where the name is all there is.
        assert!(classify(&t, Ownership::UnverifiedComm).is_empty());
    }

    /// The impersonation the `comm` rule cannot see: a shell copied to a file
    /// named `brokkr` exempted everything under it. Keyed on the verified
    /// holder PID instead, the same tree is entirely stray.
    #[test]
    fn a_process_merely_named_brokkr_does_not_exempt_its_cargo() {
        let t = table(&[
            (1, 0, "systemd"),
            (10, 1, "zsh"),
            (20, 10, "brokkr"), // the impostor; the real holder is elsewhere
            (30, 20, "cargo"),
            (40, 30, "rustc"),
        ]);
        let pids: Vec<u32> =
            classify(&t, Ownership::Holder(99)).iter().map(|s| s.pid).collect();
        assert_eq!(pids, vec![40, 30]);
        // The old rule is what the fallback preserves, spoof and all.
        assert!(classify(&t, Ownership::UnverifiedComm).is_empty());
    }

    /// A guard that outlives the cargo above it is itself reapable now.
    #[test]
    fn the_rustc_guard_is_in_the_cargo_family() {
        assert!(is_cargo_family("brokkr-rustc-gu"));
        assert!(is_cargo_family("brokkr-rustc-guard"));
        // brokkr itself must never be reapable.
        assert!(!is_cargo_family("brokkr"));
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
        let strays = classify(&t, Ownership::UnverifiedComm);
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
        let strays = classify(&t, Ownership::UnverifiedComm);
        let starters: Vec<u32> = starters_to_kill(&strays).into_iter().map(|(pid, _)| pid).collect();
        assert_eq!(starters, vec![10]);
    }

    #[test]
    fn reap_line_is_one_line_with_comm_counts_and_starter() {
        let t = table(&[
            (1, 0, "systemd"),
            (10, 1, "rust-analyzer"),
            (20, 10, "cargo"),
            (30, 20, "build_script_bu"),
            (31, 20, "build_script_bu"),
        ]);
        let strays = classify(&t, Ownership::UnverifiedComm);
        let line = reap_line(&strays, 3, 1);
        assert!(!line.contains('\n'));
        assert_eq!(
            line,
            "SIGKILL sent to 3 stray cargo processes (build_script_bu x2, cargo) started by rust-analyzer and to 1 rust-analyzer (`brokkr man check strays`)"
        );
    }

    #[test]
    fn own_process_tree_reads_without_panicking() {
        // Whatever is running on the test host, the scan must be total.
        let _ = find();
    }
}
