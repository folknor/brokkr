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
//! 1. `--brokkr-guard-info` alone - answer the version handshake and exit.
//! 2. The invocation classifies as a compiler-information query - exec, no
//!    lease, no capability asked. Queries compile nothing, and refusing one
//!    poisons cargo's persistent rustc-info cache (see [`classify`]).
//! 3. `BROKKR_CARGO` set and non-empty - exec. The manual escape hatch.
//! 4. Take a shared compile lease, read the published capability hash under
//!    it, then probe the brokkr lock (`$HOME/.brokkr/brokkr.lock`) last:
//!    not held - exec; held - exec only with a matching `BROKKR_HOLD_NONCE`,
//!    refuse (exit 1) otherwise.
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

    // The version handshake, recognized before anything else - no lease, no
    // lock probe, no environment reads - so a probing brokkr gets an answer
    // even when the lock infrastructure is wedged. An OLD guard reaching this
    // point instead treats the flag as the executable to wrap and dies at
    // `exec` with status 127, which the prober reads as "stale".
    // `brokkr guard` and locked-command startup compare this against
    // `guard::GUARD_PROTOCOL`; both sides must be edited together.
    if real == "--brokkr-guard-info" && rest.is_empty() {
        println!("brokkr-rustc-guard protocol=1 capabilities=capability-lease-v1,query-bypass-v1");
        return;
    }

    // Compiler-information queries are admitted unconditionally, before any
    // lease or lock question is asked. Two reasons, both load-bearing:
    //
    // - A query compiles nothing (the classifier only accepts invocation
    //   shapes rustc answers before expansion and codegen), so refusing it
    //   buys no exclusion for a measurement.
    // - Cargo PERSISTS a failed `rustc -vV`/`--print` probe - stderr and all -
    //   in `target/.rustc_info.json`, keyed by a fingerprint that is blind to
    //   inherited env vars. A refusal here therefore poisons the shared probe
    //   cache: every later cargo with the same fingerprint, brokkr's own
    //   correctly-stamped children included, replays the refusal without ever
    //   spawning this guard, and neither the capability nor BROKKR_CARGO can
    //   cure it. Demonstrated live 2026-09-10 against a piners `cargo doc`
    //   script check.
    //
    // No lease is taken and no BROKKR_COMPILE_LEASE marker is set: the
    // invariant is that every invocation CAPABLE of compilation participates,
    // and a query holding a lease would only delay drains. An inherited
    // marker from an admitted ancestor rides along untouched.
    if classify(&rest) == Invocation::Query {
        let mut cmd = Command::new(&real);
        cmd.args(&rest);
        let err = cmd.exec();
        eprintln!("brokkr-rustc-guard: exec {} failed: {err}", real.to_string_lossy());
        std::process::exit(127);
    }

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
                "brokkr-rustc-guard: refusing to compile - {why}. `brokkr lock` shows the \
                 holder; BROKKR_CARGO=1 overrides."
            );
            // A refused invocation carrying a `--print` request is one
            // `classify` did not recognize as a query - which means cargo may
            // PERSIST this refusal in target/.rustc_info.json and replay it
            // later, holder or no holder, hatch or no hatch. Say so in the
            // message itself, because the message is exactly what gets
            // replayed: the reader of the cached copy sees no hold and no way
            // this text should apply, and this line is their explanation.
            if rest.iter().any(|a| {
                a.to_str().is_some_and(|s| s == "--print" || s.starts_with("--print="))
            }) {
                eprintln!(
                    "brokkr-rustc-guard: note: cargo may cache this refusal in \
                     target/.rustc_info.json and replay it after the hold ends; if you are \
                     reading this with no brokkr running, re-probe with \
                     CARGO_CACHE_RUSTC_INFO=0 BROKKR_CARGO=1"
                );
            }
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

/// What the wrapped invocation is, decided from its arguments alone.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
enum Invocation {
    /// A compiler-information query that cannot reach codegen. Admitted
    /// without a capability or a lease - see the comment at the call site.
    Query,
    /// Anything else, including anything this parser does not understand.
    /// Takes the full capability/lease path.
    Compile,
}

/// `--print` requests rustc answers and exits on, before expansion, analysis
/// or codegen. `link-args` and `native-static-libs` are deliberately ABSENT:
/// rustc services those by continuing into compilation and printing at the
/// end, so an invocation carrying them is a compile wearing a query's hat.
/// Unknown print names fail closed into `Compile` for the same reason.
const EARLY_PRINTS: &[&str] = &[
    "file-names",
    "sysroot",
    "target-libdir",
    "host-tuple",
    "crate-name",
    "crate-root-lint-levels",
    "cfg",
    "check-cfg",
    "calling-conventions",
    "target-list",
    "target-cpus",
    "target-features",
    "relocation-models",
    "code-models",
    "tls-models",
    "target-spec-json",
    "all-target-specs-json",
    "split-debuginfo",
    "deployment-target",
    "stack-protector-strategies",
    "supported-crate-types",
];

/// Long options (`--opt value` / `--opt=value`) a probe legitimately carries
/// and that cannot, by themselves, request codegen or output.
const VALUE_OPTS: &[&str] = &[
    "--print",
    "--crate-name",
    "--crate-type",
    "--edition",
    "--target",
    "--cfg",
    "--check-cfg",
    "--cap-lints",
    "--error-format",
    "--json",
    "--color",
    "--diagnostic-width",
    "--sysroot",
    "--allow",
    "--warn",
    "--deny",
    "--forbid",
    "--force-warn",
];

/// Classify the wrapped rustc invocation by parsing its option grammar.
///
/// The shape recognized as a query is the one cargo actually sends when
/// probing a compiler: version requests (`rustc -vV`), and `--print` batteries
/// such as `rustc - --crate-name ___ --print=file-names --crate-type bin ...
/// --print=cfg -Wwarnings`, possibly with the user's RUSTFLAGS appended
/// (`-Ctarget-cpu`, `--cfg`, `-Z...`), a `--target`, and stdin (`-`) as the
/// sole input. Anything with a real source file, an output request (`-o`,
/// `--out-dir`, `--emit`), an unexpanded `@argfile` (rustc would re-read a
/// mutable file after this decision - a classification race), a non-early
/// print, or any option this parser does not know is `Compile`. The failure
/// direction is deliberate: misreading a query as a compile costs one
/// refusal-and-repoke, misreading a compile as a query would let real work
/// run outside the exclusion guarantee.
fn classify(args: &[std::ffi::OsString]) -> Invocation {
    let mut strs = Vec::with_capacity(args.len());
    for a in args {
        match a.to_str() {
            Some(s) => strs.push(s),
            None => return Invocation::Compile,
        }
    }

    // Version-only requests: every arg is a version/verbosity flag and at
    // least one actually requests the version.
    let version_flag = |s: &str| matches!(s, "-vV" | "-V" | "--version");
    if strs.iter().any(|s| version_flag(s))
        && strs.iter().all(|s| version_flag(s) || matches!(*s, "-v" | "--verbose"))
    {
        return Invocation::Query;
    }

    let mut prints: Vec<String> = Vec::new();
    let mut positionals = 0usize;
    let mut stdin_input = false;
    let mut i = 0;
    while i < strs.len() {
        let arg = strs[i];
        i += 1;
        if arg == "-" {
            stdin_input = true;
            positionals += 1;
            continue;
        }
        if arg == "--" {
            // Everything after is positional input; a query has none beyond
            // the one stdin marker handled above.
            positionals += strs.len() - i;
            i = strs.len();
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            let dashed = format!("--{name}");
            if !VALUE_OPTS.contains(&dashed.as_str()) {
                return Invocation::Compile;
            }
            let value = match attached {
                Some(v) => v.to_owned(),
                None => {
                    let Some(next) = strs.get(i) else {
                        return Invocation::Compile;
                    };
                    i += 1;
                    (*next).to_owned()
                }
            };
            if name == "print" {
                prints.push(value);
            }
            continue;
        }
        if let Some(short) = arg.strip_prefix('-') {
            // Lint, codegen, unstable and search-path flags: value attached
            // (`-Wwarnings`, `-Ctarget-cpu=native`) or separate (`-W warnings`).
            // A probe inherits these from RUSTFLAGS; none of them turns a
            // print request into a compile, because the early print exits
            // first. Any other short flag is unknown here and fails closed.
            let mut chars = short.chars();
            let letter = chars.next();
            if matches!(letter, Some('W' | 'A' | 'D' | 'F' | 'C' | 'Z' | 'L' | 'l')) {
                if chars.next().is_none() {
                    // Separated form consumes the next arg as its value.
                    if strs.get(i).is_none() {
                        return Invocation::Compile;
                    }
                    i += 1;
                }
                continue;
            }
            return Invocation::Compile;
        }
        if arg.starts_with('@') {
            return Invocation::Compile;
        }
        positionals += 1;
    }

    let inputs_ok = positionals == 0 || (positionals == 1 && stdin_input);
    if inputs_ok
        && !prints.is_empty()
        && prints.iter().all(|p| EARLY_PRINTS.contains(&p.as_str()))
    {
        return Invocation::Query;
    }
    Invocation::Compile
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

#[cfg(test)]
mod classify_tests {
    use super::{classify, Invocation};

    fn args(list: &[&str]) -> Vec<std::ffi::OsString> {
        list.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn version_probe_is_a_query() {
        assert_eq!(classify(&args(&["-vV"])), Invocation::Query);
        assert_eq!(classify(&args(&["--version", "--verbose"])), Invocation::Query);
        // Verbosity alone requests nothing.
        assert_eq!(classify(&args(&["-v"])), Invocation::Compile);
    }

    #[test]
    fn cargos_target_probe_is_a_query() {
        // The observed shape, RUSTFLAGS included, that poisoned the cache
        // when refused.
        assert_eq!(
            classify(&args(&[
                "-",
                "--crate-name",
                "___",
                "--print=file-names",
                "--crate-type",
                "bin",
                "--crate-type",
                "rlib",
                "--crate-type",
                "proc-macro",
                "--print=sysroot",
                "--print=split-debuginfo",
                "--print=crate-name",
                "--print=cfg",
                "-Wwarnings",
                "-Zthreads=8",
                "-Ctarget-cpu=native",
                "-Clink-arg=-fuse-ld=wild",
                "--cfg=some_flag",
            ])),
            Invocation::Query
        );
        assert_eq!(
            classify(&args(&["--print=target-spec-json", "-Zunstable-options", "--target", "x86_64-unknown-linux-gnu"])),
            Invocation::Query
        );
    }

    #[test]
    fn compilation_continuing_prints_stay_gated() {
        // rustc services these by compiling first; they are not queries.
        assert_eq!(classify(&args(&["main.rs", "--print=link-args"])), Invocation::Compile);
        assert_eq!(
            classify(&args(&["-", "--crate-type=staticlib", "--print=native-static-libs"])),
            Invocation::Compile
        );
        // Unknown print names fail closed.
        assert_eq!(classify(&args(&["--print=whatever-new-mode"])), Invocation::Compile);
    }

    #[test]
    fn real_compiles_are_never_queries() {
        assert_eq!(
            classify(&args(&["--crate-name", "foo", "src/lib.rs", "--emit=dep-info,metadata"])),
            Invocation::Compile
        );
        // A source file alongside an early print is still an input.
        assert_eq!(classify(&args(&["src/lib.rs", "--print=cfg"])), Invocation::Compile);
        // Output requests, argfiles and unknown options fail closed.
        assert_eq!(classify(&args(&["--print=cfg", "--out-dir", "x"])), Invocation::Compile);
        assert_eq!(classify(&args(&["@args.txt", "--print=cfg"])), Invocation::Compile);
        assert_eq!(classify(&args(&["--print=cfg", "--mystery-flag"])), Invocation::Compile);
        // Positionals after `--` are inputs.
        assert_eq!(classify(&args(&["--print=cfg", "--", "lib.rs"])), Invocation::Compile);
        // No print request at all: nothing was asked, assume the worst.
        assert_eq!(classify(&args(&["--crate-name", "foo"])), Invocation::Compile);
    }

    #[test]
    fn dangling_value_options_fail_closed() {
        assert_eq!(classify(&args(&["--print"])), Invocation::Compile);
        assert_eq!(classify(&args(&["--print=cfg", "-W"])), Invocation::Compile);
        // Non-UTF-8 anywhere is not parsed, so not admitted.
        use std::os::unix::ffi::OsStringExt;
        let bad = std::ffi::OsString::from_vec(vec![0x2d, 0xff, 0xfe]);
        assert_eq!(classify(&[bad]), Invocation::Compile);
    }
}
