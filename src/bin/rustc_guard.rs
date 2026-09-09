//! `brokkr-rustc-guard`: the `build.rustc-wrapper` that refuses to compile
//! while brokkr holds the benchmark lock.
//!
//! The stray reap (`src/stray.rs`) kills cargo-family processes after the
//! fact; this is the prevention side. Enrolled user-wide by `brokkr guard
//! --install` as `build.rustc-wrapper` in `$CARGO_HOME/config.toml`, so every
//! cargo this user runs - rust-analyzer's, an agent harness's, a hand-typed
//! one, absolute path or not - funnels each rustc invocation through here.
//! Cargo probes `rustc -vV` through the wrapper before building anything, so
//! a refused build dies at startup, before a single crate compiles.
//!
//! The wrapper is uniform on purpose: brokkr's own cargo goes through it too,
//! and the gate is decided at runtime, never by unsetting the wrapper. The
//! wrapper's identity is part of cargo's compile fingerprint (cargo #9348),
//! so a brokkr that bypassed it by env would ping-pong full rebuilds against
//! every other build in the same target dir.
//!
//! Decision, in order:
//! 1. `BROKKR_CARGO` set and non-empty - exec. The manual escape hatch.
//! 2. A `brokkr` ancestor in `/proc` - exec. The same rule that defines a
//!    stray, inverted: brokkr-owned rustc is whatever runs under brokkr.
//! 3. The brokkr lock (`$HOME/.brokkr/brokkr.lock`) held - refuse, exit 1.
//! 4. Otherwise - exec.
//!
//! Fail-open everywhere except a demonstrably held flock: an unreadable
//! `/proc`, a missing `$HOME`, a missing lock file all exec. The guard is a
//! benchmark-hygiene fence, not a security boundary - a wrong refusal breaks
//! someone's build, a wrong pass costs one reap cycle.
//!
//! Self-contained: no clap, no history row, no brokkr crate (the package has
//! no lib target). The `/proc` stat parse mirrors `src/stray.rs`.

use std::os::unix::process::CommandExt;
use std::process::Command;

fn main() {
    let mut args = std::env::args_os();
    let _self = args.next();
    let Some(real) = args.next() else {
        eprintln!("brokkr-rustc-guard: no wrapped command given (cargo invokes this as `guard <rustc> <args...>`)");
        std::process::exit(2);
    };
    let rest: Vec<std::ffi::OsString> = args.collect();

    if !allowed() {
        eprintln!(
            "brokkr-rustc-guard: brokkr holds the benchmark lock - refusing to compile outside brokkr \
             (`brokkr lock` shows the holder; BROKKR_CARGO=1 overrides)"
        );
        std::process::exit(1);
    }

    let err = Command::new(&real).args(&rest).exec();
    eprintln!("brokkr-rustc-guard: exec {} failed: {err}", real.to_string_lossy());
    std::process::exit(127);
}

fn allowed() -> bool {
    if std::env::var_os("BROKKR_CARGO").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    if has_brokkr_ancestor() {
        return true;
    }
    !lock_is_held()
}

/// Whether any ancestor of this process is `brokkr`, by comm. The walk reads
/// `/proc/<pid>/stat` upward from the parent; any parse failure ends the walk
/// as "no" and defers to the lock probe. Bounded like `stray.rs`'s walk - a
/// ppid cycle exists only in an inconsistent snapshot, but the bound is free.
fn has_brokkr_ancestor() -> bool {
    // SAFETY: getppid has no failure mode.
    let mut pid = unsafe { libc::getppid() };
    for _ in 0..256 {
        if pid <= 1 {
            return false;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // comm sits in parentheses and may itself contain spaces or
        // parentheses, so split at the LAST `)`; ppid is the second field
        // after it. Same parse as src/stray.rs.
        let Some(open) = stat.find('(') else { return false };
        let Some(close) = stat.rfind(')') else { return false };
        if &stat[open + 1..close] == "brokkr" {
            return true;
        }
        let mut fields = stat[close + 2..].split_whitespace();
        let _state = fields.next();
        let Some(ppid) = fields.next().and_then(|p| p.parse::<i32>().ok()) else {
            return false;
        };
        if ppid == pid {
            return false;
        }
        pid = ppid;
    }
    false
}

/// Probe the brokkr lock without taking it: a non-blocking *shared* flock on
/// `$HOME/.brokkr/brokkr.lock`. Shared, so concurrent guard probes never
/// conflict with each other, only with brokkr's exclusive hold; non-blocking,
/// so the probe conflicts rather than queues. The fd drops (and any
/// momentary shared hold releases) before the caller execs. A missing file
/// or unset `$HOME` reads as not held.
fn lock_is_held() -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    let path = std::path::PathBuf::from(home).join(".brokkr").join("brokkr.lock");
    let Ok(file) = std::fs::File::open(&path) else {
        return false;
    };
    use std::os::fd::AsRawFd;
    // SAFETY: flock on an fd owned by `file`, which outlives the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    if rc == 0 {
        // Shared lock granted, so no exclusive holder. The momentary hold
        // releases when `file` drops, before the caller execs.
        return false;
    }
    // Only EWOULDBLOCK demonstrates a conflicting owner. Every other flock
    // failure - EINTR, ENOLCK, EBADF - says nothing about whether brokkr is
    // running, and treating them as "held" inverts this guard's stated
    // fail-open policy: a kernel out of lock records would refuse every
    // compile on the machine. Fail open and let the stray reaper catch what
    // slips.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
}
