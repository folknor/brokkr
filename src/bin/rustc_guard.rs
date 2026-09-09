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

use std::os::unix::io::RawFd;
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

    let lease = match decide() {
        Decision::Admitted(lease) => Some(lease),
        Decision::Override | Decision::Idle => None,
        Decision::Unleased(why) => {
            // Fail open, and say so. This execution is outside the exclusion
            // guarantee: brokkr may be measuring alongside it and cannot know.
            eprintln!(
                "brokkr-rustc-guard: {why}; compiling without a lease, outside brokkr's \
                 exclusion guarantee"
            );
            None
        }
        Decision::Refused { lease, why } => {
            drop(lease);
            eprintln!(
                "brokkr-rustc-guard: refusing to compile - {why}. A brokkr hold is active and \
                 admits only compilers it started. `brokkr lock` shows the holder; \
                 BROKKR_CARGO=1 overrides."
            );
            std::process::exit(1);
        }
    };

    let mut cmd = Command::new(&real);
    cmd.args(&rest);
    // Tell any brokkr started from inside this compilation - a proc macro that
    // shells out - to refuse a fresh hold rather than deadlock waiting to drain
    // the very lease its own ancestor is holding.
    if let Some(lease) = lease {
        cmd.env("BROKKR_COMPILE_LEASE", "1");
        // The descriptor must outlive this process so the exec'd compiler holds
        // the share for its whole run.
        lease.leak();
    }
    let err = cmd.exec();
    eprintln!("brokkr-rustc-guard: exec {} failed: {err}", real.to_string_lossy());
    std::process::exit(127);
}

/// `$HOME/.brokkr`, the one directory both locks live in.
fn brokkr_dir() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("HOME")?).join(".brokkr"))
}

/// The published capability hash for the live hold, or `None` when the lock file
/// is unreadable or malformed.
///
/// Read while holding the shared compile lease, so the value cannot be
/// republished under us. Parsed first-occurrence-wins, matching the writer's
/// format: identity and `auth` are emitted first and byte-identical across a
/// holder's rewrites, so a torn read cannot blend two holders' capabilities.
fn read_auth() -> Option<String> {
    let text = std::fs::read_to_string(brokkr_dir()?.join("brokkr.lock")).ok()?;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("auth=") {
            return Some(v.trim().to_owned());
        }
    }
    None
}

/// The lock-file value for a nonce. Duplicated from `crate::hold::auth_hash`
/// because this binary is self-contained - the package has no lib target - in
/// the same way the `/proc` parse was duplicated from `stray.rs`. Both sides
/// must agree byte for byte, so keep them edited together.
fn auth_hash(nonce: &str) -> String {
    let mut s = String::with_capacity(32);
    for b in xxhash_rust::xxh3::xxh3_128(nonce.as_bytes()).to_be_bytes() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The lease this compilation holds, kept alive across `exec`.
///
/// Dropping this releases the share, so it is deliberately leaked on the
/// admitted path: the descriptor must outlive the guard and be held by the
/// compiler itself, which is what makes a drain wait for real compilation
/// rather than for a wrapper that has already gone.
struct Lease(RawFd);

impl Lease {
    /// Take a shared lease on the compile lock and make it survive `exec`.
    ///
    /// `None` on any infrastructure failure - the file cannot be opened, the
    /// kernel refuses the lock, `CLOEXEC` cannot be cleared. All three mean
    /// this execution will not participate in the protocol, and the caller
    /// fails open: see [`allowed`].
    fn take() -> Option<Self> {
        let path = brokkr_dir()?.join("compile.lock");
        // Created if absent, and never unlinked or replaced by anyone: the
        // drain's proof is an exclusive flock on this *inode*, so a fresh inode
        // at the same path would split the exclusion in two.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .ok()?;
        use std::os::fd::{AsRawFd, IntoRawFd};
        // SAFETY: flock on an fd owned by `file`, live for the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) } != 0 {
            return None;
        }
        let fd = file.into_raw_fd();
        // Clear CLOEXEC so the exec'd compiler inherits the share. flock
        // records live on the open file description, so the lock rides through
        // exec and is released by the kernel when the last holder of that
        // description exits - crash included, with nothing to clean up.
        //
        // A failure here is not survivable as an admission: we would exec a
        // compiler believing it leased when it did not, which is precisely the
        // overlap the lease exists to prevent. Release and let the caller fail
        // open explicitly instead.
        // SAFETY: fd is open and owned by us until `exec` or `release`.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags == -1 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                libc::flock(fd, libc::LOCK_UN);
                libc::close(fd);
                return None;
            }
        }
        Some(Self(fd))
    }

    /// Keep the descriptor open past this process, so the compiler we are about
    /// to `exec` holds the share for its whole run.
    ///
    /// The default is [`Drop`] releasing it, so every path that does *not* admit
    /// gives the lease back without having to remember to - a refused guard must
    /// never sit on a share and delay the drain it just lost to.
    fn leak(self) {
        std::mem::forget(self);
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // SAFETY: fd is ours and still open; this type is not copyable and
        // `leak` is the only way to skip this.
        unsafe {
            libc::flock(self.0, libc::LOCK_UN);
            libc::close(self.0);
        }
    }
}

/// Why this rustc was admitted or refused. Carried so the refusal can name the
/// rule rather than making the next reader reconstruct it - the absence of that
/// is what made the last failure of this fence expensive to diagnose.
enum Decision {
    /// The human escape hatch. Outside the exclusion guarantee by design.
    Override,
    /// Leased and the capability matched the live hold.
    Admitted(Lease),
    /// No hold is active.
    Idle,
    /// The lease could not be established at all. Fails open, and this
    /// execution is outside the exclusion guarantee.
    Unleased(&'static str),
    /// A hold is active and this process does not carry its capability.
    Refused { lease: Lease, why: &'static str },
}

fn decide() -> Decision {
    // First, and before any lease: a hatch that needs working lock
    // infrastructure is useless exactly when it is needed.
    if std::env::var_os("BROKKR_CARGO").is_some_and(|v| !v.is_empty()) {
        return Decision::Override;
    }

    // Lease first, then check. Taking the share before reading the capability
    // is what makes the read meaningful: a new hold cannot republish the
    // capability without the exclusive lock this share excludes, so the value
    // we read cannot change under us. Reading first and locking second is the
    // stale-read race that sank two earlier designs.
    let Some(lease) = Lease::take() else {
        return Decision::Unleased("the compile lease could not be taken");
    };

    let published = read_auth().unwrap_or_default();

    // The flock is the sole authority on whether a hold exists, and it is
    // consulted *last* so it has the final word - the window between reading the
    // record and probing is as small as it can be.
    //
    // Asking about the record first and the flock only as a fallback is a bug
    // this cost a probe to find: abnormal termination leaves the metadata behind
    // while the kernel releases the flock, so a crashed brokkr publishes a
    // perfectly well-formed capability that matches nobody. Deciding on the
    // record alone refused every compile on the machine until something
    // rewrote the file. A leftover record is inert; only a live flock is a hold.
    if !lock_is_held() {
        return Decision::Idle;
    }

    // A hold is active. An empty `auth` means it is still draining and admitting
    // nobody; a non-empty one must match the capability we carry.
    if published.is_empty() {
        return Decision::Refused {
            lease,
            why: "a brokkr hold is starting up and is not admitting compilers yet",
        };
    }
    match std::env::var("BROKKR_HOLD_NONCE") {
        Ok(nonce) if !nonce.is_empty() && auth_hash(&nonce) == published => {
            Decision::Admitted(lease)
        }
        Ok(_) => Decision::Refused {
            lease,
            why: "this process carries a capability for a different hold",
        },
        Err(_) => Decision::Refused {
            lease,
            why: "this process carries no brokkr capability",
        },
    }
}

// The `/proc` ancestry walk that used to admit "anything under a process named
// brokkr" lived here. It is gone rather than kept as a fallback, for two
// reasons. It is spoofable - a shell copied to a file named `brokkr` admitted
// every compiler beneath it - and it is an inference about process shape, so it
// fails wherever shape is not visible: a PID namespace, a restricted `/proc`, a
// child that reparents. Its failure direction was the worst available, refusing
// the one process that must be allowed, because for brokkr's own builds the lock
// is always held. The inherited capability answers the same question without
// asking the kernel to describe the process tree.

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
