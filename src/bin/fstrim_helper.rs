//! `brokkr-fstrim`: the one privileged step of a brokkr trim, `syncfs` then
//! `FITRIM` on a filesystem brokkr hands over as an open directory.
//!
//! Installed once by the operator, root-owned at a fixed path with
//! `cap_sys_admin` as a file capability (`brokkr man fstrim`). It resolves no
//! pathname: the target is the directory descriptor brokkr opened and
//! validated, so the filesystem checked is the one trimmed. Its authority is
//! whole-filesystem sync and trim through any ext4 directory descriptor its
//! caller can obtain, which is the authority the install grants the
//! operator's group.
//!
//! The brokkr flock rides in on a second descriptor, a duplicate of the
//! holder's open file description, kept open until this process exits: the
//! lock stays held while the trim runs even if brokkr dies. A parent-death
//! SIGKILL, set here because exec of a capability binary clears the
//! inherited one, ends a trim whose brokkr is gone at ext4's next
//! fatal-signal checkpoint. Neither descriptor proves the caller is brokkr:
//! the `fdinfo` lock check is a diagnostic, reported and never refused on.
//!
//! Self-contained: std and libc, no brokkr crate. Every line it prints is
//! one stage's result, so a caller that stops it keeps every stage that
//! finished.
//!
//! It refuses a protocol other than its own and, before any stage, a run
//! without CAP_SYS_ADMIN effective.
//!
//! Usage: `brokkr-fstrim --probe`, or
//! `brokkr-fstrim --trim PROTOCOL PPID DIR_FD LOCK_FD`.

use std::io::Write as _;

/// The line protocol's version, first on every run.
const PROTOCOL: u32 = 1;

const EXT4_SUPER_MAGIC: i64 = 0xEF53;
const CAP_SYS_ADMIN: u32 = 21;
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// `_IOWR('X', 121, struct fstrim_range)`.
const FITRIM: libc::c_ulong = 0xC018_5879;

#[repr(C)]
struct FstrimRange {
    start: u64,
    len: u64,
    minlen: u64,
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["--probe"] => probe(),
        ["--trim", protocol, ppid, dir, lock] => {
            match (protocol.parse::<u32>(), ppid.parse(), dir.parse(), lock.parse()) {
                (Ok(p), _, _, _) if p != PROTOCOL => {
                    refuse(&format!("protocol {p} requested, this helper speaks {PROTOCOL}"))
                }
                (Ok(_), Ok(ppid), Ok(dir), Ok(lock)) => trim(ppid, dir, lock),
                _ => refuse("arguments are a protocol, a parent pid and two descriptor numbers"),
            }
        }
        _ => refuse("usage: brokkr-fstrim --probe | --trim PROTOCOL PPID DIR_FD LOCK_FD"),
    };
    std::process::exit(code);
}

fn line(text: &str) {
    let mut out = std::io::stdout().lock();
    // A closed stdout leaves the caller nothing to read; the exit code still
    // says how the run ended.
    writeln!(out, "{text}").ok();
    out.flush().ok();
}

fn refuse(why: &str) -> i32 {
    line(&format!("refused {why}"));
    2
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn now_ns() -> u128 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid timespec for the call's duration.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &raw mut ts) };
    u128::try_from(ts.tv_sec).unwrap_or(0) * 1_000_000_000 + u128::try_from(ts.tv_nsec).unwrap_or(0)
}

/// Whether CAP_SYS_ADMIN is in this process's effective set.
fn has_sys_admin() -> Option<bool> {
    let mut header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [CapData::default(); 2];
    // SAFETY: a version-3 header and the two data structs it names.
    let rc = unsafe { libc::syscall(libc::SYS_capget, &raw mut header, data.as_mut_ptr()) };
    (rc == 0).then(|| data[0].effective & (1 << CAP_SYS_ADMIN) != 0)
}

fn probe() -> i32 {
    line(&format!("brokkr-fstrim {PROTOCOL}"));
    match has_sys_admin() {
        Some(true) => {
            line("cap_sys_admin effective");
            0
        }
        Some(false) => {
            line("cap_sys_admin absent");
            1
        }
        None => {
            line(&format!("cap_sys_admin unknown {}", errno()));
            1
        }
    }
}

/// Whether `/proc/self/fdinfo/FD` lists an exclusive flock on that open
/// file description; `None` when procfs could not tell.
fn holds_flock(fd: i32) -> Option<bool> {
    let text = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).ok()?;
    Some(text.lines().any(|l| {
        l.starts_with("lock:") && l.contains("FLOCK") && l.contains("WRITE")
    }))
}

fn trim(ppid: i32, dir: i32, lock: i32) -> i32 {
    // SAFETY: plain prctl and getppid calls.
    unsafe {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
            return refuse(&format!("pdeathsig {}", errno()));
        }
        if libc::getppid() != ppid {
            return refuse("parent gone");
        }
    }
    line(&format!("brokkr-fstrim {PROTOCOL}"));
    if has_sys_admin() != Some(true) {
        return refuse("cap_sys_admin is not effective");
    }
    // SAFETY: zeroed out-structs for fstat and fstatfs.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(dir, &raw mut st) } != 0 {
        return refuse(&format!("target {}", errno()));
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return refuse("target is not a directory");
    }
    // SAFETY: as above.
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(dir, &raw mut sfs) } != 0 {
        return refuse(&format!("target {}", errno()));
    }
    if sfs.f_type != EXT4_SUPER_MAGIC {
        return refuse(&format!("fstype 0x{:x}", sfs.f_type));
    }
    match holds_flock(lock) {
        Some(true) => line("lock held"),
        _ => line("lock unverified"),
    }
    // SAFETY: f_fsid is two plain ints however libc names its fields.
    let fsid: [i32; 2] = unsafe { std::mem::transmute(sfs.f_fsid) };
    line(&format!(
        "target {}:{} {:08x}{:08x}",
        libc::major(st.st_dev),
        libc::minor(st.st_dev),
        fsid[0].cast_unsigned(),
        fsid[1].cast_unsigned()
    ));

    let start = now_ns();
    // SAFETY: syncfs on a descriptor validated above.
    if unsafe { libc::syncfs(dir) } != 0 {
        line(&format!("sync failed {}", errno()));
        return 1;
    }
    line(&format!("sync ok {}", now_ns() - start));

    let mut range = FstrimRange {
        start: 0,
        len: u64::MAX,
        minlen: 0,
    };
    let start = now_ns();
    // SAFETY: FITRIM with a valid fstrim_range for the call's duration.
    if unsafe { libc::ioctl(dir, FITRIM as _, &raw mut range) } != 0 {
        line(&format!("trim failed {}", errno()));
        return 1;
    }
    line(&format!("trim ok {} {}", range.len, now_ns() - start));
    0
}
