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
//! Both this reaper and the orphan reaper spare the same process-tree boundary:
//! this process, its visible ancestors and its visible descendants. This
//! reaper also protects a verified lock holder and its namespace. Ownership is
//! decided by process identities, never by an ancestor being named `brokkr`;
//! see [`Ownership`] for why a name is not enough. Signals are
//! identity-checked against a typed process generation, and a stray's descendant
//! tree dies with it, stopping at anything brokkr owns.

use std::collections::{HashMap, HashSet};

use crate::error::DevError;
use crate::output;
pub(crate) use crate::process::ReaperExemptions;

const ANCESTRY_DEPTH_LIMIT: usize = 256;
const DESCENDANT_DEPTH_LIMIT: usize = 64;

/// One cargo-family process selected for cleanup outside brokkr's protected
/// invocation and ownership trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stray {
    pub pid: u32,
    /// Typed process generation captured in the same proc row as its parent.
    pub identity: crate::process::ProcIdentity,
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

/// The stray reaper's extra ownership proofs, beyond the shared process-tree
/// boundary in [`ReaperExemptions`].
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
    /// The holder's PID namespace, when it runs sandboxed relative to this
    /// process (its namespace is not ours). Then its PID cannot be verified
    /// from here, so `holder` is `None`, and the namespace stands in for it:
    /// every process in that namespace or below it is the holder's. A sandbox
    /// that gives each command its own PID namespace (codex) holds exactly the
    /// holder's workload there. Never set when the holder shares our namespace
    /// - that would exempt every process on the host.
    foreign_ns: Option<u64>,
}

impl Ownership {
    /// Whether a PID is protected by the shared process-tree rule or the
    /// lock-holder / namespace ownership proof.
    fn owns(&self, pid: u32, exemptions: &ReaperExemptions) -> bool {
        exemptions.contains(pid) || self.owns_subtree_root(pid)
    }

    /// Whether this PID itself is brokkr, a separately verified lock holder,
    /// or inside the verified holder namespace. Used while walking ancestry so
    /// an ancestor protected by the shared policy does not also classify all
    /// of its other branches as brokkr-owned.
    fn owns_subtree_root(&self, pid: u32) -> bool {
        pid == self.me
            || self.holder == Some(pid)
            || self.foreign_ns.is_some_and(|ns| within_ns(pid, ns))
    }
}

/// Whether `pid` lives in PID namespace `ns` or one nested below it. Walks
/// `NS_GET_PARENT` up from the process's own namespace; the walk ends with
/// `EPERM` at this process's namespace, which means "not within". Any other
/// failure to inspect the process answers `true`: exempting a process we
/// cannot classify is the conservative direction for a reaper.
fn within_ns(pid: u32, ns: u64) -> bool {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    // _IO(0xb7, 0x2)
    const NS_GET_PARENT: libc::c_ulong = 0xb702;
    let Ok(file) = std::fs::File::open(format!("/proc/{pid}/ns/pid")) else {
        return true;
    };
    let mut current: OwnedFd = file.into();
    // Bounded: the kernel nests PID namespaces at most 32 deep.
    for _ in 0..33 {
        // SAFETY: fstat on an fd we own into a zeroed stat buffer.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(current.as_raw_fd(), &mut st) } != 0 {
            return true;
        }
        if st.st_ino == ns {
            return true;
        }
        // SAFETY: an ioctl on an fd we own; it returns a fresh fd or -1.
        let parent = unsafe { libc::ioctl(current.as_raw_fd(), NS_GET_PARENT) };
        if parent < 0 {
            return std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM);
        }
        // SAFETY: a successful NS_GET_PARENT returns a new fd we own.
        current = unsafe { OwnedFd::from_raw_fd(parent) };
    }
    true
}

/// The ownership rule in force right now, read from the lock file.
fn current_ownership() -> Ownership {
    let info = crate::lockfile::status().ok().flatten();
    let holder = info.as_ref().and_then(|info| {
        let identity = info.process_identity()?;
        let boot = crate::process::BootId::new(&info.boot_id)?;
        (crate::process::shares_namespace_tokens(&info.pid_ns, &info.time_ns)
            && crate::process::verify_identity(identity, &boot))
        .then_some(info.pid)
    });
    let foreign_ns = info
        .as_ref()
        .and_then(|info| crate::process::foreign_pid_namespace(&info.pid_ns))
        .map(crate::process::NsInode::get);
    Ownership {
        me: std::process::id(),
        holder,
        foreign_ns,
    }
}

struct ProcEntry {
    ppid: u32,
    comm: String,
    identity: crate::process::ProcIdentity,
}

fn read_proc() -> HashMap<u32, ProcEntry> {
    let mut table = HashMap::new();
    let Ok(snapshot) = crate::process::ProcTable::snapshot() else {
        return table;
    };
    for (pid, stat) in snapshot.iter() {
        table.insert(
            pid.get(),
            ProcEntry {
                ppid: stat.parent().map_or(0, crate::process::Pid::get),
                comm: stat.comm().to_owned(),
                identity: stat.identity(),
            },
        );
    }
    table
}

/// Every unprotected cargo-family process, leaves first. Pure over the given
/// table and ownership rule so the classification is unit-testable.
fn classify(table: &HashMap<u32, ProcEntry>, ownership: Ownership) -> Vec<Stray> {
    let exemptions = ReaperExemptions::from_parent_pairs(
        ownership.me,
        table.iter().map(|(&pid, entry)| (pid, entry.ppid)),
    );
    let mut strays = Vec::new();
    for (&pid, entry) in table {
        if !is_cargo_family(&entry.comm) || ownership.owns(pid, &exemptions) {
            continue;
        }
        let mut under_brokkr = false;
        let mut started_by = None;
        let mut starter = None;
        let mut depth = 0usize;
        let mut cursor = entry.ppid;
        // Bounded walk: a `/proc` snapshot can contain a ppid cycle only if
        // the table is inconsistent, but a bound costs nothing.
        for _ in 0..ANCESTRY_DEPTH_LIMIT {
            let Some(parent) = table.get(&cursor) else {
                break;
            };
            depth += 1;
            if ownership.owns_subtree_root(cursor) {
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
            identity: entry.identity,
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
    exemptions: &ReaperExemptions,
    seen: &mut HashSet<u32>,
    out: &mut Vec<(usize, crate::process::ProcIdentity)>,
) {
    if depth > DESCENDANT_DEPTH_LIMIT || ownership.owns(pid, exemptions) {
        return;
    }
    for (&child, entry) in table {
        if entry.ppid != pid || child == pid || ownership.owns(child, exemptions) {
            continue;
        }
        if !seen.insert(child) {
            continue;
        }
        out.push((depth + 1, entry.identity));
        collect_descendants(table, child, depth + 1, ownership, exemptions, seen, out);
    }
}

/// The strays on this host right now, leaves first.
pub fn find() -> Vec<Stray> {
    classify(&read_proc(), current_ownership())
}

/// The two groups `kill` may signal. A process that exited between the scan
/// and signal is not counted and not an error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KillCounts {
    pub strays_signalled: usize,
    pub starters_signalled: usize,
}

/// SIGKILL each stray, then each rust-analyzer starter.
pub fn kill(strays: &[Stray]) -> KillCounts {
    // Re-read `/proc` once: it supplies both the fresh identity tokens the
    // signals are checked against and the descendant topology. A single read
    // keeps the two coherent.
    let table = read_proc();
    let ownership = current_ownership();
    let exemptions = ReaperExemptions::from_parent_pairs(
        ownership.me,
        table.iter().map(|(&pid, entry)| (pid, entry.ppid)),
    );

    // Signal only if it is still the process generation we recorded.
    // A bare PID is not an identity: between classification and the kill the
    // process can exit and the number be reused, and then this signals something
    // it never classified.
    let sigkill = |identity: crate::process::ProcIdentity| {
        let pid = identity.pid().get();
        // Every target, descendants and starters included, not just the
        // classified strays: an owned process is never signalled.
        if ownership.owns(pid, &exemptions) {
            return false;
        }
        matches!(
            crate::process::kill_verified(identity, libc::SIGKILL),
            Ok(crate::process::Delivery::Signalled)
        )
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
    let mut descendants: Vec<(usize, crate::process::ProcIdentity)> = Vec::new();
    for s in strays {
        collect_descendants(
            &table,
            s.pid,
            0,
            &ownership,
            &exemptions,
            &mut seen,
            &mut descendants,
        );
    }
    descendants.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.pid().cmp(&b.1.pid())));
    for (_, identity) in &descendants {
        if !strays.iter().any(|s| s.pid == identity.pid().get()) {
            sigkill(*identity);
        }
    }
    let killed = strays.iter().filter(|s| sigkill(s.identity)).count();
    let starters = starters_to_kill(strays)
        .into_iter()
        .filter(|(pid, _)| table.get(pid).is_some_and(|entry| sigkill(entry.identity)))
        .count();
    KillCounts {
        strays_signalled: killed,
        starters_signalled: starters,
    }
}

/// The `SIGKILL sent to …` line.
fn killed_line(counts: KillCounts) -> String {
    let mut line = format!(
        "SIGKILL sent to {}",
        output::count(counts.strays_signalled, "stray cargo process")
    );
    if counts.starters_signalled > 0 {
        line.push_str(&format!(
            " and {} (it would only re-run the cargo)",
            output::count(counts.starters_signalled, "rust-analyzer")
        ));
    }
    line
}

/// The reap's whole report, one line however many strays there were - the
/// reap runs at the top of every locked command, so its output is overhead
/// paid on every invocation. Comms are counted (`build_script_bu x4`),
/// starters deduplicated. No pids: everything named is dead by the time the
/// line prints, so a pid identifies nothing. The live, addressable detail
/// stays with `brokkr strays`.
fn reap_line(strays: &[Stray], counts: KillCounts) -> String {
    let mut comms: Vec<(&str, usize)> = Vec::new();
    for s in strays {
        match comms.iter_mut().find(|(c, _)| *c == s.comm) {
            Some((_, n)) => *n += 1,
            None => comms.push((&s.comm, 1)),
        }
    }
    let comms: Vec<String> = comms
        .iter()
        .map(|(c, n)| {
            if *n > 1 {
                format!("{c} x{n}")
            } else {
                (*c).to_owned()
            }
        })
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
        output::count(counts.strays_signalled, "stray cargo process"),
        comms.join(", "),
        by.join(", "),
    );
    if counts.starters_signalled > 0 {
        line.push_str(&format!(
            " and to {}",
            output::count(counts.starters_signalled, "rust-analyzer")
        ));
    }
    line
}

/// One report line per stray.
pub fn describe(s: &Stray) -> String {
    format!("{} (pid {}) started by {}", s.comm, s.pid, s.started_by)
}

/// The reap every locked command runs once it holds the lock: find, kill,
/// report on one line. Called from `lockfile::acquire`'s fresh-hold path -
/// not from any one command wrapper - so no caller of the lock can skip it. Nothing found prints nothing. Failure to read `/proc`
/// reads as nothing found - the reap is a convenience on the way to the real
/// work, never a gate on it.
pub fn reap_after_lock() {
    let strays = find();
    if strays.is_empty() {
        return;
    }
    let counts = kill(&strays);
    output::strays_msg(&reap_line(&strays, counts));
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
    let counts = kill(&strays);
    output::strays_msg(&reap_line(&strays, counts));
}

/// `brokkr strays [--kill]`: bare lists, `--kill` lists then kills.
pub fn cmd_strays(kill_them: bool) -> Result<(), DevError> {
    let strays = find();
    if strays.is_empty() {
        output::strays_msg("no stray cargo processes");
        return Ok(());
    }
    for s in &strays {
        output::strays_msg(&describe(s));
    }
    if kill_them {
        let counts = kill(&strays);
        output::strays_msg(&killed_line(counts));
    } else {
        output::strays_msg(
            "`brokkr strays --kill` sends SIGKILL; every locked brokkr command does so on its own once it holds the lock (`brokkr man check strays`)",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn table(rows: &[(u32, u32, &str)]) -> HashMap<u32, ProcEntry> {
        rows.iter()
            .map(|&(pid, ppid, comm)| {
                (
                    pid,
                    ProcEntry {
                        ppid,
                        comm: comm.to_owned(),
                        identity: crate::process::ProcIdentity::new(
                            crate::process::Pid::new(pid).unwrap(),
                            crate::process::StartTicks::new(u64::from(pid) * 100),
                        ),
                    },
                )
            })
            .collect()
    }

    /// No hold, and nothing of ours in the tree.
    /// A process is within its own PID namespace, and the parent walk stops at
    /// this process's namespace with "not within" for any other inode.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn namespace_containment_walks_to_our_own_namespace() {
        let me = std::process::id();
        let own = crate::process::namespace_inode("/proc/self/ns/pid")
            .unwrap()
            .get();
        assert!(within_ns(me, own));
        assert!(!within_ns(me, own.wrapping_add(1)));
        let o = Ownership {
            me: 1,
            holder: None,
            foreign_ns: Some(own),
        };
        let exemptions = ReaperExemptions::from_parent_pairs(me, std::iter::empty());
        assert!(o.owns(me, &exemptions));
    }

    fn foreign() -> Ownership {
        Ownership {
            me: 999_999,
            holder: None,
            foreign_ns: None,
        }
    }

    #[test]
    fn cargo_under_the_holder_is_not_a_stray() {
        let t = table(&[
            (1, 0, "systemd"),
            (10, 1, "zsh"),
            (20, 10, "brokkr"),
            (30, 20, "cargo"),
            (40, 30, "rustc"),
        ]);
        assert!(
            classify(
                &t,
                Ownership {
                    me: 999_999,
                    holder: Some(20),
                    foreign_ns: None
                }
            )
            .is_empty()
        );
        // And when the holder *is* this process.
        assert!(
            classify(
                &t,
                Ownership {
                    me: 20,
                    holder: Some(20),
                    foreign_ns: None
                }
            )
            .is_empty()
        );
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
        let unverifiable: Vec<u32> = classify(
            &t,
            Ownership {
                me: 999_999,
                holder: None,
                foreign_ns: None,
            },
        )
        .iter()
        .map(|s| s.pid)
        .collect();
        assert_eq!(unverifiable, vec![40, 30]);
    }

    /// `cargo(caller) -> brokkr(us) -> cargo(ours)`: both caller and our work
    /// are inside the shared self/ancestor/descendant exemption boundary.
    #[test]
    fn stray_reaping_spares_our_ancestors_and_descendants() {
        let t = table(&[
            (1, 0, "systemd"),
            (10, 1, "cargo"),   // our caller
            (20, 10, "brokkr"), // us, launched from that cargo
            (30, 20, "cargo"),  // our own work
            (40, 30, "rustc"),
        ]);
        let own = Ownership {
            me: 20,
            holder: Some(20),
            foreign_ns: None,
        };
        // Neither the caller Cargo nor our child Cargo is a stray.
        let pids: Vec<u32> = classify(&t, own).iter().map(|s| s.pid).collect();
        assert!(pids.is_empty());

        let exemptions = ReaperExemptions::from_parent_pairs(
            own.me,
            t.iter().map(|(&pid, entry)| (pid, entry.ppid)),
        );
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        collect_descendants(&t, 10, 0, &own, &exemptions, &mut seen, &mut out);
        let reached: Vec<u32> = out
            .iter()
            .map(|(_, identity)| identity.pid().get())
            .collect();
        assert!(
            reached.is_empty(),
            "descendant expansion must stop at the shared exemption boundary: {reached:?}"
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
        let exemptions = ReaperExemptions::from_parent_pairs(
            999_999,
            t.iter().map(|(&pid, entry)| (pid, entry.ppid)),
        );
        // Both roots cover 30; it must appear once.
        collect_descendants(&t, 10, 0, &foreign(), &exemptions, &mut seen, &mut out);
        collect_descendants(&t, 20, 0, &foreign(), &exemptions, &mut seen, &mut out);
        let mut pids: Vec<u32> = out
            .iter()
            .map(|(_, identity)| identity.pid().get())
            .collect();
        pids.sort_unstable();
        assert_eq!(pids, vec![20, 30]);
    }

    /// Every stray carries the typed process generation captured with its
    /// parent row.
    #[test]
    fn strays_keep_the_typed_identity_for_their_pid() {
        let t = table(&[
            (1, 0, "systemd"),
            (10, 1, "rust-analyzer"),
            (20, 10, "cargo"),
        ]);
        let strays = classify(&t, foreign());
        assert_eq!(strays.len(), 1);
        assert_eq!(strays[0].identity.start().get(), 2_000);
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
        assert!(
            strays
                .iter()
                .all(|s| s.started_by == "rust-analyzer (pid 10)"),
            "{strays:?}"
        );
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
        let starters: Vec<u32> = starters_to_kill(&strays)
            .into_iter()
            .map(|(pid, _)| pid)
            .collect();
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
        let line = reap_line(
            &strays,
            KillCounts {
                strays_signalled: 3,
                starters_signalled: 1,
            },
        );
        assert!(!line.contains('\n'));
        assert_eq!(
            line,
            "SIGKILL sent to 3 stray cargo processes (build_script_bu x2, cargo) started by rust-analyzer and to 1 rust-analyzer"
        );
    }

    #[test]
    fn own_process_tree_reads_without_panicking() {
        // Whatever is running on the test host, the scan must be total.
        let _ = find();
    }
}
