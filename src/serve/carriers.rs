//! The processes of a served program, found by the token they carry.
//!
//! The supervisor stamps `BROKKR_SERVE=<token>` on the server, and the serve
//! contract is that every process of a served program keeps it in its
//! environment, stays readable in `/proc` and stays in the launcher's PID
//! namespace. A scan of `/proc` then finds them wherever they reparented and
//! whatever process group they joined.
//!
//! A scan is not an absence proof: `/proc` enumeration is not atomic, so a
//! carrier that forks a child and exits while the scan passes it can leave
//! the child unobserved. What it gives is a best-effort cleanup that is
//! conservative on every uncertainty it can see: a scan that cannot read the
//! process table is [`Scan::Unknown`], never an empty one.

use crate::process::{Pid, ProcIdentity, ProcStat};

/// The variable carrying a served program's generation token.
pub(crate) const SERVE_ENV: &str = "BROKKR_SERVE";

/// One process carrying a generation's token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Carrier {
    pub(crate) identity: ProcIdentity,
    pub(crate) comm: String,
    /// The scanning process, an ancestor or a descendant of it: counted as
    /// outstanding, never signalled.
    pub(crate) exempt: bool,
}

/// What a scan found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scan {
    Known(Vec<Carrier>),
    Unknown(String),
}

/// The token in a raw `/proc/<pid>/environ` block, first occurrence winning
/// as it does for `getenv`.
fn token_in_environ(environ: &[u8]) -> Option<&[u8]> {
    let prefix = format!("{SERVE_ENV}=");
    environ
        .split(|&b| b == 0)
        .find_map(|entry| entry.strip_prefix(prefix.as_bytes()))
}

/// Which carriers a scan marks exempt from signalling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exempt {
    /// The scanning process, its ancestors and its descendants: a reader is
    /// never the one to kill the process tree it runs in, whatever token it
    /// carries.
    Family,
    /// None: the supervisor's own sweep, whose descendants are exactly the
    /// workload it owns (the supervisor itself never carries the token).
    Nothing,
}

/// Every visible process carrying `token`. Each environment read is bound to
/// the generation it was read from: the row is read again afterwards and
/// discarded when its starttime moved, so a recycled PID never lends one
/// process's token to another's identity.
pub(crate) fn scan(token: &str, exempt: Exempt) -> Scan {
    let table = match crate::process::ProcTable::snapshot() {
        Ok(table) => table,
        Err(e) => return Scan::Unknown(format!("cannot read the process table: {e}")),
    };
    let me = std::process::id();
    let exemptions = crate::process::ReaperExemptions::from_parent_pairs(
        me,
        table
            .iter()
            .map(|(pid, stat)| (pid.get(), stat.parent().map_or(0, Pid::get))),
    );
    let mut out = Vec::new();
    for (pid, stat) in table.iter() {
        let Ok(environ) = std::fs::read(format!("/proc/{}/environ", pid.get())) else {
            continue;
        };
        if token_in_environ(&environ) != Some(token.as_bytes()) {
            continue;
        }
        let Ok(again) = ProcStat::read(*pid) else {
            continue;
        };
        if again.start() != stat.start() || matches!(again.state(), 'Z' | 'X') {
            continue;
        }
        out.push(Carrier {
            identity: stat.identity(),
            comm: stat.comm().to_owned(),
            exempt: exempt == Exempt::Family && exemptions.contains(pid.get()),
        });
    }
    Scan::Known(out)
}

/// Scan-and-SIGKILL passes before a sweep reports what is left.
const PASSES: usize = 3;
/// How long a pass waits for what it signalled to finish dying.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(1);

/// SIGKILL every non-exempt carrier of `token`, each through a pidfd opened
/// between two verifications of its starttime, rescanning between passes so
/// a child forked meanwhile is caught next round; return the final scan.
pub(crate) fn sweep(token: &str, exempt: Exempt) -> Scan {
    let mut last = scan(token, exempt);
    for _ in 0..PASSES {
        let Scan::Known(carriers) = &last else {
            return last;
        };
        let targets: Vec<ProcIdentity> = carriers
            .iter()
            .filter(|c| !c.exempt)
            .map(|c| c.identity)
            .collect();
        if targets.is_empty() {
            return last;
        }
        for identity in &targets {
            crate::process::kill_verified(*identity, libc::SIGKILL).ok();
        }
        let deadline = std::time::Instant::now() + SETTLE;
        while std::time::Instant::now() < deadline
            && targets.iter().any(|id| {
                ProcStat::read(id.pid())
                    .is_ok_and(|s| s.start() == id.start() && !matches!(s.state(), 'Z' | 'X'))
            })
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        last = scan(token, exempt);
    }
    last
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    #[test]
    fn the_token_is_read_from_a_raw_environ_block() {
        let block = b"A=1\0BROKKR_SERVE=abc\0BROKKR_SERVE=def\0";
        assert_eq!(token_in_environ(block), Some(&b"abc"[..]));
        assert_eq!(token_in_environ(b"BROKKR_SERVER=x\0"), None);
    }

    /// A real carrier is found; a reader's sweep counts but spares it, being
    /// this process's child; the owner's sweep kills it and leaves another
    /// token's carrier alone.
    #[test]
    fn a_sweep_kills_the_carriers_of_its_token_only() {
        let token = crate::lockfile::RunToken::mint().unwrap();
        let other = crate::lockfile::RunToken::mint().unwrap();
        let spawn = |t: &str| {
            let mut cmd = std::process::Command::new("sleep");
            cmd.arg("30")
                .env(SERVE_ENV, t)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            crate::process::spawn::ChildPolicy::test_process().apply_runtime_policy(&mut cmd);
            crate::process::spawn::spawn(cmd, None).unwrap()
        };
        let mut mine = spawn(token.as_str());
        let mut theirs = spawn(other.as_str());
        let found = match scan(token.as_str(), Exempt::Family) {
            Scan::Known(found) => found,
            Scan::Unknown(why) => panic!("{why}"),
        };
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].identity.pid().get(), mine.id());
        // A child of this process is its descendant: exempt.
        assert!(found[0].exempt);
        // The exemption keeps it counted and unsignalled.
        assert_eq!(
            sweep(token.as_str(), Exempt::Family),
            scan(token.as_str(), Exempt::Family)
        );
        assert!(mine.try_wait().unwrap().is_none());
        // The owner's sweep spares nothing. Its child is a zombie until
        // reaped, which a scan does not count.
        assert_eq!(
            sweep(token.as_str(), Exempt::Nothing),
            Scan::Known(Vec::new())
        );
        assert!(mine.wait().unwrap().code().is_none(), "killed by a signal");
        assert!(theirs.try_wait().unwrap().is_none());
        theirs.kill().ok();
        theirs.wait().ok();
    }
}
