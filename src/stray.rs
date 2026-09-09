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
//! stray - is decided by two pids, this process and the verified lock holder,
//! never by an ancestor being named `brokkr`; see [`Ownership`] for why a name
//! is not enough. Signals are identity-checked against a recorded starttime,
//! and a stray's descendant tree dies with it, stopping at anything brokkr owns.

use std::collections::{HashMap, HashSet};

use crate::error::DevError;
use crate::output;

/// One cargo-family process with no brokkr ancestor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stray {
    pub pid: u32,
    /// `/proc/<pid>/stat` starttime, the identity token that makes the PID
    /// safe to signal later.
    ///
    /// A PID alone is not an identity: between classification and the SIGKILL
    /// the process can exit and the number be reused, and then the reaper
    /// signals something it never classified - potentially brokkr itself or any
    /// unrelated process of this user. `src/lockfile.rs` has paired every PID
    /// with a starttime for exactly this reason since it was written; this path
    /// did not, and that was a real hole rather than a theoretical one.
    pub starttime: String,
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

/// Which process trees are brokkr's own work, and therefore not strays.
///
/// Two PIDs, never a name. Ancestry by `comm` was the original rule and it is
/// spoofable in the way that matters: any executable *named* `brokkr` exempted
/// every cargo and rustc beneath it, with no reference to whether it held
/// anything. A shell copied to that name is enough - which is how this fence was
/// first probed. It is gone rather than kept as a fallback, because the case that
/// justified keeping it (never SIGKILL brokkr's own build) needs no lock file at
/// all: this reaper runs *inside* brokkr, so it can identify its own work by its
/// own PID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ownership {
    /// This process. Its subtree is brokkr's own work by definition, and
    /// identifying it requires reading nothing.
    me: u32,
    /// The verified current lock holder, when there is one and its identity
    /// checks out. During a reap from inside acquisition this *is* `me`; it
    /// differs only for a hand-run `brokkr strays` while another brokkr holds.
    ///
    /// `None` when no hold is active, or when a hold exists whose identity
    /// cannot be verified from this namespace. That second case used to fall
    /// back to the name rule, which let an executable named `brokkr` shield
    /// foreign cargo from a drain that then expired for want of killing it.
    /// Unverifiable now means unprotected - except for `me`, which is what the
    /// fallback was really protecting.
    holder: Option<u32>,
}

impl Ownership {
    /// Whether an ancestor PID marks the tree below it as brokkr's own.
    fn owns(&self, pid: u32) -> bool {
        pid == self.me || self.holder == Some(pid)
    }
}

/// The ownership rule in force right now, read from the lock file.
fn current_ownership() -> Ownership {
    let holder = match crate::lockfile::status() {
        Ok(Some(info))
            if crate::lockfile::verify_identity(info.pid, &info.starttime, &info.boot_id) =>
        {
            Some(info.pid)
        }
        _ => None,
    };
    Ownership { me: std::process::id(), holder }
}

struct ProcEntry {
    ppid: u32,
    comm: String,
    /// Field 22 of `/proc/<pid>/stat`, the PID's identity token.
    starttime: String,
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
        let post: Vec<&str> = stat[close + 2..].split_whitespace().collect();
        // Post-comm field 0 is state, 1 is ppid; starttime is field 22 of the
        // whole line, which is index 19 here. Same indexing as
        // `lockfile::proc_starttime`, and read from the *same* line as ppid so
        // the pair is coherent.
        let Some(ppid) = post.first().and_then(|_| post.get(1)).and_then(|p| p.parse().ok())
        else {
            continue;
        };
        let starttime = post.get(19).map(|s| (*s).to_owned()).unwrap_or_default();
        table.insert(pid, ProcEntry { ppid, comm, starttime });
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
            if ownership.owns(cursor) {
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
            starttime: entry.starttime.clone(),
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

/// Every descendant of `pid` that brokkr does not own, with its depth and
/// identity token.
///
/// `seen` is shared across every root, so overlapping stray trees traverse each
/// process once - without it, overlapping roots re-walked and re-signalled the
/// same subtrees, and an inconsistent snapshot could amplify that up to the
/// depth cap. Termination comes from `seen` plus the depth bound; a parent cycle
/// exists only in an inconsistent snapshot, and either guard alone would stop it.
///
/// Anything [`Ownership::owns`] recognises is skipped *and not recursed through*,
/// so brokkr and the lock holder keep their whole subtrees.
fn collect_descendants(
    table: &HashMap<u32, ProcEntry>,
    pid: u32,
    depth: usize,
    ownership: &Ownership,
    seen: &mut HashSet<u32>,
    out: &mut Vec<(usize, u32, String)>,
) {
    if depth > 64 {
        return;
    }
    for (&child, entry) in table {
        if entry.ppid != pid || child == pid || ownership.owns(child) {
            continue;
        }
        if !seen.insert(child) {
            continue;
        }
        out.push((depth + 1, child, entry.starttime.clone()));
        collect_descendants(table, child, depth + 1, ownership, seen, out);
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
    // Re-read `/proc` once: it supplies both the fresh identity tokens the
    // signals are checked against and the descendant topology. A single read
    // keeps the two coherent.
    let table = read_proc();
    let ownership = current_ownership();

    // Signal `pid` only if it is still the process whose starttime we recorded.
    // A bare PID is not an identity: between classification and the kill the
    // process can exit and the number be reused, and then this signals something
    // it never classified.
    let sigkill = |pid: u32, expect: &str| {
        if expect.is_empty() {
            return false;
        }
        match crate::lockfile::proc_starttime(pid) {
            Some(now) if now == expect => {
                // SAFETY: identity re-verified immediately above; ESRCH is
                // benign, and the residual window is the one every
                // PID-addressed signal has.
                unsafe { libc::kill(pid.cast_signed(), libc::SIGKILL) == 0 }
            }
            _ => false,
        }
    };

    // Descendants of each stray, deepest first, before the strays themselves.
    //
    // Signalling only the selected PIDs left a hole: killing a cargo does not
    // kill the rustc-wrapper beneath it, so an already-admitted wrapper survived
    // the reap and went on to exec a compiler - the single thing the reap exists
    // to prevent. A stray can also have children of any name, and a build
    // script's children are arbitrary programs.
    //
    // Expansion stops at anything brokkr owns and never recurses through it.
    // Without that, `cargo(stray) -> brokkr(holder) -> ...` was fatal: the
    // classifier walking up from the cargo cannot see the holder *below* it, so
    // the cargo is a stray, and expanding its descendants then reached brokkr
    // itself and SIGKILLed it. That topology occurs whenever brokkr is launched
    // from a cargo.
    //
    // This is not atomic containment - a process can fork again between the read
    // and the signal - so it narrows the hole rather than closing it. Closing it
    // needs a cgroup, which is a larger change than this reaper is.
    let mut seen: HashSet<u32> = HashSet::new();
    let mut descendants: Vec<(usize, u32, String)> = Vec::new();
    for s in strays {
        collect_descendants(&table, s.pid, 0, &ownership, &mut seen, &mut descendants);
    }
    descendants.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, pid, starttime) in &descendants {
        if !strays.iter().any(|s| s.pid == *pid) {
            sigkill(*pid, starttime);
        }
    }
    let killed = strays.iter().filter(|s| sigkill(s.pid, &s.starttime)).count();
    let starters = starters_to_kill(strays)
        .into_iter()
        .filter(|(pid, _)| {
            table.get(pid).is_some_and(|e| sigkill(*pid, &e.starttime))
        })
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
            .map(|&(pid, ppid, comm)| {
                (
                    pid,
                    ProcEntry {
                        ppid,
                        comm: comm.to_owned(),
                        // Distinct per pid, so a test that checks identity
                        // verification cannot pass by accident.
                        starttime: format!("{pid}00"),
                    },
                )
            })
            .collect()
    }

    /// No hold, and nothing of ours in the tree.
    fn foreign() -> Ownership {
        Ownership { me: 999_999, holder: None }
    }

    #[test]
    fn cargo_under_the_holder_is_not_a_stray() {
        let t = table(&[(1, 0, "systemd"), (10, 1, "zsh"), (20, 10, "brokkr"), (30, 20, "cargo"), (40, 30, "rustc")]);
        assert!(classify(&t, Ownership { me: 999_999, holder: Some(20) }).is_empty());
        // And when the holder *is* this process.
        assert!(classify(&t, Ownership { me: 20, holder: Some(20) }).is_empty());
    }

    /// The impersonation the `comm` rule could not see: a shell copied to a file
    /// named `brokkr` exempted everything under it. Keyed on PIDs brokkr can
    /// actually vouch for, the same tree is entirely stray - including when a
    /// hold exists whose identity cannot be verified, which used to fall back to
    /// the name and shield foreign cargo from a drain.
    #[test]
    fn a_process_merely_named_brokkr_does_not_exempt_its_cargo() {
        let t = table(&[
            (1, 0, "systemd"),
            (10, 1, "zsh"),
            (20, 10, "brokkr"), // the impostor; the real holder is elsewhere
            (30, 20, "cargo"),
            (40, 30, "rustc"),
        ]);
        let pids: Vec<u32> = classify(&t, foreign()).iter().map(|s| s.pid).collect();
        assert_eq!(pids, vec![40, 30]);
        let unverifiable: Vec<u32> = classify(&t, Ownership { me: 999_999, holder: None })
            .iter()
            .map(|s| s.pid)
            .collect();
        assert_eq!(unverifiable, vec![40, 30]);
    }

    /// `cargo(stray) -> brokkr(us) -> cargo(ours)`: the classifier walking up
    /// from the outer cargo cannot see us below it, so the outer cargo is a
    /// stray. Expanding its descendants must not reach us or our work.
    #[test]
    fn descendant_expansion_never_crosses_into_brokkrs_own_subtree() {
        let t = table(&[
            (1, 0, "systemd"),
            (10, 1, "cargo"),   // the stray root
            (20, 10, "brokkr"), // us, launched from that cargo
            (30, 20, "cargo"),  // our own work
            (40, 30, "rustc"),
        ]);
        let own = Ownership { me: 20, holder: Some(20) };
        // The outer cargo is a stray; ours is not.
        let pids: Vec<u32> = classify(&t, own).iter().map(|s| s.pid).collect();
        assert_eq!(pids, vec![10]);

        let mut seen = HashSet::new();
        let mut out = Vec::new();
        collect_descendants(&t, 10, 0, &own, &mut seen, &mut out);
        let reached: Vec<u32> = out.iter().map(|(_, pid, _)| *pid).collect();
        assert!(
            reached.is_empty(),
            "expansion from a stray must stop at brokkr, not walk through it: {reached:?}"
        );
    }

    /// Overlapping roots must traverse each process once.
    #[test]
    fn descendant_expansion_visits_each_process_once() {
        let t = table(&[
            (1, 0, "systemd"),
            (10, 1, "cargo"),
            (20, 10, "build_script_bu"),
            (30, 20, "cc"),
        ]);
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        // Both roots cover 30; it must appear once.
        collect_descendants(&t, 10, 0, &foreign(), &mut seen, &mut out);
        collect_descendants(&t, 20, 0, &foreign(), &mut seen, &mut out);
        let mut pids: Vec<u32> = out.iter().map(|(_, pid, _)| *pid).collect();
        pids.sort_unstable();
        assert_eq!(pids, vec![20, 30]);
    }

    /// Every stray carries the identity token its later SIGKILL is checked
    /// against.
    #[test]
    fn strays_record_the_identity_token_for_their_pid() {
        let t = table(&[(1, 0, "systemd"), (10, 1, "rust-analyzer"), (20, 10, "cargo")]);
        let strays = classify(&t, foreign());
        assert_eq!(strays.len(), 1);
        assert_eq!(strays[0].starttime, "2000", "the token must come from the same /proc read");
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
        let strays = classify(&t, foreign());
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
        let strays = classify(&t, foreign());
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
        let strays = classify(&t, foreign());
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
